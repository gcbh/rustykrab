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
        }
    }

    /// Use a custom base profile that gets task-specific overlays applied.
    pub fn with_base(mut self, base: HarnessProfile) -> Self {
        self.base = base;
        self
    }

    /// Classify the complexity of a user message for pipeline routing.
    ///
    /// Returns a TaskComplexity that determines which orchestration
    /// pipeline stages to run.
    pub async fn classify_complexity(&self, user_message: &str) -> TaskComplexity {
        let truncated = truncate_for_classification(user_message);
        let prompt = format!("{COMPLEXITY_PROMPT}{truncated}");

        let messages = vec![Message {
            id: Uuid::new_v4(),
            role: Role::User,
            content: MessageContent::Text(prompt),
            created_at: chrono::Utc::now(),
        }];

        match self.classifier.chat(&messages, &[]).await {
            Ok(response) => {
                let text = response
                    .message
                    .content
                    .as_text()
                    .unwrap_or("simple")
                    .trim()
                    .to_lowercase();
                parse_complexity(&text)
            }
            Err(e) => {
                tracing::debug!("complexity classification failed: {e}");
                TaskComplexity::Simple
            }
        }
    }

    /// Classify a user message and return the appropriate harness profile.
    ///
    /// On classification failure (model error, unparseable response), falls
    /// back to the base profile rather than blocking the request.
    pub async fn route(&self, user_message: &str) -> HarnessProfile {
        match self.classify(user_message).await {
            Ok(task_type) => {
                let mut profile = self.profile_for(task_type);
                // Preserve any user customizations from the base profile.
                profile.agent_name = self.base.agent_name.clone();
                profile.max_context_tokens = self.base.max_context_tokens;
                profile
            }
            Err(e) => {
                tracing::debug!("harness classification failed, using default: {e}");
                self.base.clone()
            }
        }
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
}
