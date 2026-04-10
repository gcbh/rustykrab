//! Context budgeting for recursive calls.
//!
//! Ensures no single model call exceeds its context budget by
//! tracking token usage and splitting work across sub-calls.

use rustykrab_core::orchestration::OrchestrationConfig;

/// Manages context budgets across the recursive call tree.
pub struct ContextManager {
    config: OrchestrationConfig,
}

impl ContextManager {
    pub fn new(config: OrchestrationConfig) -> Self {
        Self { config }
    }

    /// Calculate the context budget for a child call at a given depth.
    ///
    /// Budget decreases at deeper levels to prevent runaway resource usage.
    /// Each level gets ~75% of the parent's budget.
    pub fn child_budget(&self, parent_budget: usize, depth: usize) -> usize {
        let max_depth = self.config.max_recursion_depth;
        if depth >= max_depth {
            return 0;
        }

        // Each level gets 75% of the parent's budget.
        let factor = 0.75_f64.powi(depth as i32);
        let budget = (parent_budget as f64 * factor) as usize;

        // Floor at 2K tokens — below that, model output quality degrades.
        budget.max(2048).min(parent_budget)
    }

    /// Estimate token count for a string (conservative: ~3.5 chars/token).
    pub fn estimate_tokens(text: &str) -> usize {
        (text.len() as f64 / 3.5).ceil() as usize
    }

    /// Check if adding text would exceed the budget.
    pub fn would_exceed(&self, current_tokens: usize, text: &str, budget: usize) -> bool {
        current_tokens + Self::estimate_tokens(text) > budget
    }

    /// Truncate text to fit within a token budget.
    pub fn truncate_to_budget(text: &str, max_tokens: usize) -> String {
        let max_chars = (max_tokens as f64 * 3.5) as usize;
        if text.len() <= max_chars {
            return text.to_string();
        }

        // Find a clean char boundary.
        let truncate_at = text
            .char_indices()
            .take_while(|(i, _)| *i < max_chars)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(max_chars.min(text.len()));

        let mut result = text[..truncate_at].to_string();
        result.push_str("… [truncated]");
        result
    }

    /// Maximum recursion depth allowed.
    pub fn max_depth(&self) -> usize {
        self.config.max_recursion_depth
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_child_budget_decreases() {
        let config = OrchestrationConfig::default();
        let parent_budget = config.sub_task_context_budget;
        let cm = ContextManager::new(config);

        let b0 = cm.child_budget(parent_budget, 0);
        let b1 = cm.child_budget(parent_budget, 1);
        let b2 = cm.child_budget(parent_budget, 2);

        assert!(b0 > b1, "depth 0 budget should be > depth 1");
        assert!(b1 > b2, "depth 1 budget should be > depth 2");
    }

    #[test]
    fn test_child_budget_at_max_depth() {
        let config = OrchestrationConfig {
            max_recursion_depth: 3,
            ..Default::default()
        };
        let parent_budget = config.sub_task_context_budget;
        let cm = ContextManager::new(config);

        assert_eq!(cm.child_budget(parent_budget, 3), 0);
        assert_eq!(cm.child_budget(parent_budget, 10), 0);
    }

    #[test]
    fn test_truncate_to_budget() {
        let text = "a".repeat(10000);
        let truncated = ContextManager::truncate_to_budget(&text, 100);
        assert!(truncated.len() < 500); // 100 tokens * 3.5 + some overhead
        assert!(truncated.ends_with("… [truncated]"));
    }

    #[test]
    fn test_truncate_short_text() {
        let text = "hello world";
        let result = ContextManager::truncate_to_budget(text, 100);
        assert_eq!(result, "hello world");
    }
}
