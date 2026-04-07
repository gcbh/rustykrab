use std::sync::Arc;

use openclaw_core::model::ModelProvider;
use openclaw_core::orchestration::TaskComplexity;
use openclaw_core::types::{Message, MessageContent, Role};
use uuid::Uuid;

use crate::harness::{HarnessProfile, TaskType};

/// Routes incoming messages to the right harness profile automatically.
///
/// Uses a cheap/fast model (Haiku-class, tiny Qwen, etc.) to classify the
/// user's intent in a single call, then returns the appropriate profile.
/// The main model never sees this overhead — the router runs before the
/// agent loop starts.
///
/// This eliminates the need to manually pick profiles. One OpenClaw instance
/// handles coding, research, creative, and general tasks seamlessly.
pub struct HarnessRouter {
    /// A fast, cheap model used only for classification.
    /// This should be the smallest model available (Haiku, Qwen 0.5B, etc.)
    classifier: Arc<dyn ModelProvider>,
    /// Base profile to use as a template. Task-specific fields get overlaid.
    base: HarnessProfile,
    /// When true, use the LLM classifier for task routing.
    /// When false (default), use instant keyword-based classification.
    /// Keyword mode is ideal for local models (Ollama) where an extra LLM
    /// call would add 30-40 seconds of latency per request.
    use_llm_classifier: bool,
}

/// Classification prompt — kept minimal to minimize latency and cost.
const CLASSIFY_PROMPT: &str = "\
Classify this user message into exactly one category. Reply with ONLY the category name, nothing else.

Categories:
- coding: writing, debugging, reviewing, or explaining code
- research: finding information, comparing options, fact-checking
- creative: writing stories, poems, marketing copy, brainstorming
- planning: project plans, task breakdowns, architecture decisions
- general: casual conversation, simple questions, everything else

User message: ";

/// Complexity classification prompt.
const COMPLEXITY_PROMPT: &str = "\
Rate the complexity of this user message. Reply with ONLY one word, nothing else.

Levels:
- trivial: simple greeting, acknowledgment, yes/no question
- simple: single fact lookup, one-step task, calendar check
- moderate: multi-step task, requires gathering info from multiple sources
- complex: needs research, synthesis, drafting with quality requirements
- critical: high-stakes decision, ambiguous situation, needs careful analysis

User message: ";

impl HarnessRouter {
    /// Create a router with a fast classifier model.
    ///
    /// ```ignore
    /// // Use the cheapest model available for classification.
    /// let haiku = Arc::new(AnthropicProvider::new(key).with_model("claude-haiku-4-5-20251001"));
    /// let router = HarnessRouter::new(haiku);
    /// let profile = router.route("Write a Python function that sorts a list").await?;
    /// assert_eq!(profile.task_type, TaskType::Coding);
    /// ```
    pub fn new(classifier: Arc<dyn ModelProvider>) -> Self {
        Self {
            classifier,
            base: HarnessProfile::default(),
            use_llm_classifier: false,
        }
    }

    /// Use a custom base profile that gets task-specific overlays applied.
    pub fn with_base(mut self, base: HarnessProfile) -> Self {
        self.base = base;
        self
    }

    /// Enable or disable LLM-based classification.
    ///
    /// When `true`, the router makes an LLM call to classify each message
    /// (better accuracy, but adds latency — significant on local models).
    /// When `false` (the default), uses instant keyword-based matching.
    ///
    /// Recommended: `false` for Ollama/local, `true` for Anthropic/cloud.
    pub fn with_llm_classifier(mut self, enabled: bool) -> Self {
        self.use_llm_classifier = enabled;
        self
    }

    /// Classify the complexity of a user message for pipeline routing.
    ///
    /// Returns a TaskComplexity that determines which orchestration
    /// pipeline stages to run. When `use_llm_classifier` is false, uses
    /// a fast heuristic instead of an LLM call.
    pub async fn classify_complexity(&self, user_message: &str) -> TaskComplexity {
        if self.use_llm_classifier {
            match self.classify_complexity_llm(user_message).await {
                Ok(c) => c,
                Err(e) => {
                    tracing::debug!("LLM complexity classification failed, falling back to keywords: {e}");
                    classify_complexity_keywords(user_message)
                }
            }
        } else {
            classify_complexity_keywords(user_message)
        }
    }

    /// Classify complexity via LLM call.
    async fn classify_complexity_llm(&self, user_message: &str) -> openclaw_core::Result<TaskComplexity> {
        let truncated = truncate_for_classification(user_message);
        let prompt = format!("{COMPLEXITY_PROMPT}{truncated}");

        let messages = vec![Message {
            id: Uuid::new_v4(),
            role: Role::User,
            content: MessageContent::Text(prompt),
            created_at: chrono::Utc::now(),
        }];

        let response = self.classifier.chat(&messages, &[]).await?;
        let text = response
            .message
            .content
            .as_text()
            .unwrap_or("simple")
            .trim()
            .to_lowercase();
        Ok(parse_complexity(&text))
    }

    /// Classify a user message and return the appropriate harness profile.
    ///
    /// When `use_llm_classifier` is true, makes an LLM call (falling back
    /// to keywords on failure). When false, uses instant keyword matching.
    pub async fn route(&self, user_message: &str) -> HarnessProfile {
        let task_type = if self.use_llm_classifier {
            match self.classify(user_message).await {
                Ok(t) => t,
                Err(e) => {
                    tracing::debug!("LLM classification failed, falling back to keywords: {e}");
                    classify_with_keywords(user_message)
                }
            }
        } else {
            classify_with_keywords(user_message)
        };

        let mut profile = self.profile_for(task_type);
        // Preserve any user customizations from the base profile.
        profile.agent_name = self.base.agent_name.clone();
        profile.max_context_tokens = self.base.max_context_tokens;
        profile
    }

    /// Classify the user's message into a TaskType.
    async fn classify(&self, user_message: &str) -> openclaw_core::Result<TaskType> {
        let truncated = truncate_for_classification(user_message);
        let prompt = format!("{CLASSIFY_PROMPT}{truncated}");

        let messages = vec![Message {
            id: Uuid::new_v4(),
            role: Role::User,
            content: MessageContent::Text(prompt),
            created_at: chrono::Utc::now(),
        }];

        let response = self.classifier.chat(&messages, &[]).await?;
        let text = response
            .message
            .content
            .as_text()
            .unwrap_or("general")
            .trim()
            .to_lowercase();

        Ok(parse_task_type(&text))
    }

    /// Get the right profile preset for a task type, starting from defaults.
    fn profile_for(&self, task_type: TaskType) -> HarnessProfile {
        match task_type {
            TaskType::Coding => HarnessProfile::coding(),
            TaskType::Research => HarnessProfile::research(),
            TaskType::Creative => HarnessProfile::creative(),
            TaskType::Planning => {
                // Planning uses the research profile with a task_type override.
                let mut p = HarnessProfile::research();
                p.name = "planning".to_string();
                p.task_type = TaskType::Planning;
                p.agent_description = "a methodical planning assistant. You break complex \
                    problems into actionable steps and identify dependencies."
                    .to_string();
                p
            }
            TaskType::General => self.base.clone(),
        }
    }
}

/// Classify a message using keyword matching. No LLM call -- instant result.
///
/// Checks the first ~300 characters of the message (lowercased) against
/// keyword lists for each category. The category with the most keyword
/// hits wins. Defaults to `General` if no keywords match.
///
/// This is the default routing strategy for local model deployments
/// (e.g., Ollama on Apple Silicon) where an LLM classification call
/// would add 30-40 seconds of latency.
fn classify_with_keywords(text: &str) -> TaskType {
    let lower = text.to_lowercase();
    let sample = if lower.len() > 300 {
        &lower[..lower.floor_char_boundary(300)]
    } else {
        &lower
    };

    // Keyword lists per category. Multi-word phrases are checked first
    // so they aren't double-counted by their individual words.
    const CODING_KEYWORDS: &[&str] = &[
        "typescript", "javascript", "python", "rust", "sql", "html", "css",
        "docker", "kubernetes", "npm", "cargo", "pip", "function", "bug",
        "compile", "error", "implement", "code", "debug", "fix", "refactor",
        "test", "class", "method", "variable", "parse", "syntax", "api",
        "endpoint", "database", "query", "migration", "deploy", "git",
        "commit", "merge", "branch",
    ];

    const RESEARCH_KEYWORDS: &[&str] = &[
        "difference between", "pros and cons", "tell me about", "how does",
        "what is", "look up", "search", "find", "compare", "explain",
        "research", "summarize", "analyze", "investigate", "review",
    ];

    const CREATIVE_KEYWORDS: &[&str] = &[
        "write a story", "write a poem", "blog post", "draft", "creative",
        "brainstorm", "imagine", "fiction", "narrative", "article",
        "marketing", "slogan", "tagline",
    ];

    const PLANNING_KEYWORDS: &[&str] = &[
        "plan", "roadmap", "architecture", "design", "strategy", "breakdown",
        "steps", "milestone", "timeline", "dependency", "prioritize",
        "schedule",
    ];

    let count = |keywords: &[&str]| -> usize {
        keywords.iter().filter(|kw| sample.contains(*kw)).count()
    };

    let coding = count(CODING_KEYWORDS);
    let research = count(RESEARCH_KEYWORDS);
    let creative = count(CREATIVE_KEYWORDS);
    let planning = count(PLANNING_KEYWORDS);

    let max = coding.max(research).max(creative).max(planning);
    if max == 0 {
        return TaskType::General;
    }

    // On ties, prefer in order: coding > research > planning > creative.
    if coding == max {
        TaskType::Coding
    } else if research == max {
        TaskType::Research
    } else if planning == max {
        TaskType::Planning
    } else {
        TaskType::Creative
    }
}

/// Classify message complexity using simple heuristics. No LLM call.
///
/// Heuristic:
/// - Under 50 chars with no sequencing language -> Trivial
/// - Contains sequencing markers ("and then", "after that", numbered steps) -> Complex
/// - Multiple sentences or mentions of multiple concepts -> Moderate
/// - Everything else -> Simple
fn classify_complexity_keywords(text: &str) -> TaskComplexity {
    let lower = text.to_lowercase();
    let char_count = lower.len();

    // Check for complex sequencing markers.
    let complex_markers = [
        "and then", "after that", "next step", "step 1", "step 2",
        "first,", "second,", "third,", "finally,", "1.", "2.", "3.",
        "1)", "2)", "3)", "multiple steps", "multi-step",
    ];
    let complex_hits: usize = complex_markers.iter().filter(|m| lower.contains(*m)).count();
    if complex_hits >= 2 {
        return TaskComplexity::Complex;
    }

    // Check for critical indicators.
    let critical_markers = ["critical", "urgent", "production", "security vulnerability", "data loss"];
    let critical_hits: usize = critical_markers.iter().filter(|m| lower.contains(*m)).count();
    if critical_hits >= 2 {
        return TaskComplexity::Critical;
    }

    // Trivial: short messages with no complexity signals.
    if char_count < 50 && complex_hits == 0 {
        return TaskComplexity::Trivial;
    }

    // Moderate: longer messages or those with some structure.
    let sentence_count = text.matches(". ").count() + text.matches("? ").count() + 1;
    if sentence_count >= 3 || char_count > 200 || complex_hits == 1 {
        return TaskComplexity::Moderate;
    }

    TaskComplexity::Simple
}

/// Truncate a message for classification (first ~200 chars).
fn truncate_for_classification(text: &str) -> &str {
    if text.len() > 200 {
        &text[..text
            .char_indices()
            .take_while(|(i, _)| *i < 200)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(200)]
    } else {
        text
    }
}

/// Parse a model response into a TaskComplexity.
fn parse_complexity(text: &str) -> TaskComplexity {
    let lower = text.to_lowercase();
    if lower.contains("trivial") {
        TaskComplexity::Trivial
    } else if lower.contains("simple") {
        TaskComplexity::Simple
    } else if lower.contains("moderate") {
        TaskComplexity::Moderate
    } else if lower.contains("complex") {
        TaskComplexity::Complex
    } else if lower.contains("critical") {
        TaskComplexity::Critical
    } else {
        TaskComplexity::Simple
    }
}

/// Parse a model response into a TaskType. Generous matching — accepts
/// partial matches, surrounding text, etc. Falls back to General.
fn parse_task_type(text: &str) -> TaskType {
    let lower = text.to_lowercase();
    if lower.contains("coding") || lower.contains("code") {
        TaskType::Coding
    } else if lower.contains("research") {
        TaskType::Research
    } else if lower.contains("creative") || lower.contains("writing") {
        TaskType::Creative
    } else if lower.contains("planning") || lower.contains("plan") {
        TaskType::Planning
    } else {
        TaskType::General
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_complexity() {
        assert_eq!(parse_complexity("trivial"), TaskComplexity::Trivial);
        assert_eq!(parse_complexity("simple"), TaskComplexity::Simple);
        assert_eq!(parse_complexity("moderate"), TaskComplexity::Moderate);
        assert_eq!(parse_complexity("complex"), TaskComplexity::Complex);
        assert_eq!(parse_complexity("critical"), TaskComplexity::Critical);
        assert_eq!(parse_complexity("unknown"), TaskComplexity::Simple);
    }

    #[test]
    fn test_parse_task_type() {
        assert_eq!(parse_task_type("coding"), TaskType::Coding);
        assert_eq!(parse_task_type("CODING"), TaskType::Coding);
        assert_eq!(parse_task_type("code"), TaskType::Coding);
        assert_eq!(parse_task_type("research"), TaskType::Research);
        assert_eq!(parse_task_type("creative"), TaskType::Creative);
        assert_eq!(parse_task_type("creative writing"), TaskType::Creative);
        assert_eq!(parse_task_type("planning"), TaskType::Planning);
        assert_eq!(parse_task_type("general"), TaskType::General);
        assert_eq!(parse_task_type("something unknown"), TaskType::General);
    }

    #[test]
    fn test_classify_keywords_coding() {
        assert_eq!(
            classify_with_keywords("fix the bug in the login function"),
            TaskType::Coding
        );
        assert_eq!(
            classify_with_keywords("implement a REST api endpoint for user query"),
            TaskType::Coding
        );
        assert_eq!(
            classify_with_keywords("refactor the database migration code"),
            TaskType::Coding
        );
        assert_eq!(
            classify_with_keywords("debug the typescript compile error"),
            TaskType::Coding
        );
    }

    #[test]
    fn test_classify_keywords_research() {
        assert_eq!(
            classify_with_keywords("what is the difference between REST and GraphQL"),
            TaskType::Research
        );
        assert_eq!(
            classify_with_keywords("compare the pros and cons of React vs Vue"),
            TaskType::Research
        );
        assert_eq!(
            classify_with_keywords("explain how DNS resolution works"),
            TaskType::Research
        );
    }

    #[test]
    fn test_classify_keywords_creative() {
        assert_eq!(
            classify_with_keywords("write a story about a robot in space"),
            TaskType::Creative
        );
        assert_eq!(
            classify_with_keywords("draft a blog post about machine learning"),
            TaskType::Creative
        );
        assert_eq!(
            classify_with_keywords("brainstorm marketing slogan ideas"),
            TaskType::Creative
        );
    }

    #[test]
    fn test_classify_keywords_planning() {
        assert_eq!(
            classify_with_keywords("create a roadmap with milestones for the project"),
            TaskType::Planning
        );
        assert_eq!(
            classify_with_keywords("plan the architecture and design for the new system"),
            TaskType::Planning
        );
        assert_eq!(
            classify_with_keywords("breakdown the timeline and prioritize tasks"),
            TaskType::Planning
        );
    }

    #[test]
    fn test_classify_keywords_general() {
        assert_eq!(
            classify_with_keywords("hello how are you"),
            TaskType::General
        );
        assert_eq!(
            classify_with_keywords("thanks for your help"),
            TaskType::General
        );
        assert_eq!(classify_with_keywords(""), TaskType::General);
    }

    #[test]
    fn test_classify_keywords_tiebreak() {
        // Coding wins ties over other categories.
        assert_eq!(
            classify_with_keywords("fix the code and explain how it works"),
            TaskType::Coding
        );
    }

    #[test]
    fn test_classify_keywords_long_message_truncated() {
        // Only the first ~300 chars should be checked.
        let padding = "a ".repeat(200);
        let msg = format!("{padding}function bug compile");
        // The keywords appear after 400 chars, so they should be ignored.
        assert_eq!(classify_with_keywords(&msg), TaskType::General);
    }

    #[test]
    fn test_classify_complexity_keywords_trivial() {
        assert_eq!(
            classify_complexity_keywords("hello"),
            TaskComplexity::Trivial
        );
        assert_eq!(
            classify_complexity_keywords("yes"),
            TaskComplexity::Trivial
        );
    }

    #[test]
    fn test_classify_complexity_keywords_simple() {
        assert_eq!(
            classify_complexity_keywords(
                "Can you help me understand how the routing system works in this project?"
            ),
            TaskComplexity::Simple
        );
    }

    #[test]
    fn test_classify_complexity_keywords_moderate() {
        assert_eq!(
            classify_complexity_keywords(
                "I need help with my project. It involves setting up a database. \
                 Then I need to connect it to the frontend. Can you help?"
            ),
            TaskComplexity::Moderate
        );
    }

    #[test]
    fn test_classify_complexity_keywords_complex() {
        assert_eq!(
            classify_complexity_keywords(
                "First, set up the database. Second, create the API. And then \
                 deploy to production after that."
            ),
            TaskComplexity::Complex
        );
    }
}
