//! REPL-style tools for recursive context exploration.
//!
//! Instead of dumping the full context into the prompt, we store it
//! externally and give the model tools to peek, search, and delegate
//! sub-queries — mirroring the foundational RLM paper's Python REPL
//! approach (Zhang, Kraska, Khattab — arXiv 2512.24601).

use std::sync::Arc;

use async_trait::async_trait;
use regex::Regex;
use rustykrab_core::model::ModelProvider;
use rustykrab_core::orchestration::OrchestrationConfig;
use rustykrab_core::tool::Tool;
use rustykrab_core::types::ToolSchema;
use serde_json::{json, Value};
use tokio::sync::Semaphore;

use super::context_manager::ContextManager;

/// Maximum characters returned by a single `context_peek` call.
const MAX_PEEK_CHARS: usize = 50_000;

/// Build the set of REPL tools for a given recursion level.
///
/// At `depth >= max_recursion_depth - 1` the `sub_query` tool is
/// omitted so the model must answer directly from peek/search results.
pub fn repl_tools(
    context: Arc<String>,
    provider: Arc<dyn ModelProvider>,
    config: OrchestrationConfig,
    depth: usize,
    semaphore: Arc<Semaphore>,
) -> Vec<Arc<dyn Tool>> {
    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(ContextInfoTool {
            context: context.clone(),
        }),
        Arc::new(ContextPeekTool {
            context: context.clone(),
        }),
        Arc::new(ContextSearchTool {
            context: context.clone(),
        }),
    ];

    if depth < config.max_recursion_depth.saturating_sub(1) {
        tools.push(Arc::new(SubQueryTool {
            context,
            provider,
            config,
            depth,
            semaphore,
        }));
    }

    tools
}

// ── context_info ────────────────────────────────────────────────────

struct ContextInfoTool {
    context: Arc<String>,
}

#[async_trait]
impl Tool for ContextInfoTool {
    fn name(&self) -> &str {
        "context_info"
    }

    fn description(&self) -> &str {
        "Get metadata about the context variable: character count, \
         estimated token count, line count, and a short preview of the \
         first 500 characters."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        }
    }

    async fn execute(&self, _args: Value) -> rustykrab_core::Result<Value> {
        let length_chars = self.context.len();
        let estimated_tokens = ContextManager::estimate_tokens(&self.context);
        let line_count = self.context.lines().count();
        let preview_end = self
            .context
            .char_indices()
            .take_while(|(i, _)| *i < 500)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(length_chars.min(500));
        let preview = &self.context[..preview_end];

        Ok(json!({
            "length_chars": length_chars,
            "estimated_tokens": estimated_tokens,
            "line_count": line_count,
            "preview": preview
        }))
    }
}

// ── context_peek ────────────────────────────────────────────────────

struct ContextPeekTool {
    context: Arc<String>,
}

#[async_trait]
impl Tool for ContextPeekTool {
    fn name(&self) -> &str {
        "context_peek"
    }

    fn description(&self) -> &str {
        "View a slice of the context by character position. Returns \
         context[start..end]. Clamped to context bounds and a safety \
         limit of 50 000 characters per call."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "start": {
                        "type": "integer",
                        "description": "Start character position (0-indexed, inclusive)"
                    },
                    "end": {
                        "type": "integer",
                        "description": "End character position (exclusive)"
                    }
                },
                "required": ["start", "end"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> rustykrab_core::Result<Value> {
        let start = args["start"].as_u64().unwrap_or(0) as usize;
        let end = args["end"].as_u64().unwrap_or(0) as usize;

        let ctx_len = self.context.len();
        let start = start.min(ctx_len);
        let end = end.min(ctx_len).max(start);

        // Snap to char boundaries.
        let start = snap_to_char_boundary(&self.context, start);
        let end = snap_to_char_boundary(&self.context, end);

        // Enforce safety limit.
        let effective_end = end.min(start + MAX_PEEK_CHARS);
        let effective_end = snap_to_char_boundary(&self.context, effective_end);

        let slice = &self.context[start..effective_end];
        let truncated = effective_end < end;

        Ok(json!({
            "text": slice,
            "start": start,
            "end": effective_end,
            "truncated": truncated
        }))
    }
}

// ── context_search ──────────────────────────────────────────────────

struct ContextSearchTool {
    context: Arc<String>,
}

#[async_trait]
impl Tool for ContextSearchTool {
    fn name(&self) -> &str {
        "context_search"
    }

    fn description(&self) -> &str {
        "Search the context using a regex pattern. Returns matching \
         lines with their line numbers and character offsets. Use this \
         to locate relevant sections before peeking at them."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Regex pattern to search for (case-insensitive)"
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Maximum number of matches to return (default 20)"
                    }
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> rustykrab_core::Result<Value> {
        let pattern = args["pattern"]
            .as_str()
            .ok_or_else(|| rustykrab_core::Error::ToolExecution("missing pattern".into()))?;
        let max_results = args["max_results"].as_u64().unwrap_or(20) as usize;

        let re = Regex::new(&format!("(?i){pattern}")).map_err(|e| {
            rustykrab_core::Error::ToolExecution(format!("invalid regex: {e}").into())
        })?;

        let mut matches = Vec::new();
        let mut char_offset = 0usize;

        for (line_num, line) in self.context.lines().enumerate() {
            if re.is_match(line) {
                matches.push(json!({
                    "line_number": line_num + 1,
                    "char_offset": char_offset,
                    "text": if line.len() > 200 {
                        format!("{}...", &line[..line.floor_char_boundary(200)])
                    } else {
                        line.to_string()
                    }
                }));
                if matches.len() >= max_results {
                    break;
                }
            }
            // +1 for the newline character.
            char_offset += line.len() + 1;
        }

        Ok(json!({
            "total_matches": matches.len(),
            "matches": matches
        }))
    }
}

// ── sub_query ───────────────────────────────────────────────────────

struct SubQueryTool {
    context: Arc<String>,
    provider: Arc<dyn ModelProvider>,
    config: OrchestrationConfig,
    depth: usize,
    semaphore: Arc<Semaphore>,
}

#[async_trait]
impl Tool for SubQueryTool {
    fn name(&self) -> &str {
        "sub_query"
    }

    fn description(&self) -> &str {
        "Launch a focused sub-LLM call on a specific slice of the \
         context. The sub-call gets its own tool set to explore the \
         slice. Use this to delegate analysis of a section you have \
         identified via context_peek or context_search. Be thoughtful \
         about batching — aim for larger slices rather than many tiny \
         calls."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "The question for the sub-call to answer"
                    },
                    "start": {
                        "type": "integer",
                        "description": "Start character position of the context slice (inclusive)"
                    },
                    "end": {
                        "type": "integer",
                        "description": "End character position of the context slice (exclusive)"
                    }
                },
                "required": ["question", "start", "end"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> rustykrab_core::Result<Value> {
        let question = args["question"]
            .as_str()
            .ok_or_else(|| rustykrab_core::Error::ToolExecution("missing question".into()))?;
        let start = args["start"].as_u64().unwrap_or(0) as usize;
        let end = args["end"].as_u64().unwrap_or(0) as usize;

        let ctx_len = self.context.len();
        let start = snap_to_char_boundary(&self.context, start.min(ctx_len));
        let end = snap_to_char_boundary(&self.context, end.min(ctx_len).max(start));

        let child_context = Arc::new(self.context[start..end].to_string());

        tracing::info!(
            depth = self.depth + 1,
            context_slice = format!("[{}..{}]", start, end),
            question_preview = &question[..question.len().min(80)],
            "RLM REPL: launching sub_query"
        );

        // Acquire semaphore permit to bound concurrent LLM calls.
        let _permit = self.semaphore.acquire().await.expect("semaphore closed");

        let answer = super::recursive_call::execute_repl_call(
            self.provider.clone(),
            self.config.clone(),
            question.to_string(),
            child_context,
            self.depth + 1,
            self.semaphore.clone(),
        )
        .await?;

        Ok(json!({ "answer": answer }))
    }
}

// ── helpers ─────────────────────────────────────────────────────────

/// Snap a byte offset forward to the nearest UTF-8 character boundary.
fn snap_to_char_boundary(s: &str, offset: usize) -> usize {
    let mut pos = offset.min(s.len());
    while pos < s.len() && !s.is_char_boundary(pos) {
        pos += 1;
    }
    pos
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_snap_to_char_boundary() {
        let s = "hello 世界";
        // "hello " is 6 bytes, "世" is 3 bytes, "界" is 3 bytes = 12 total
        assert_eq!(snap_to_char_boundary(s, 0), 0);
        assert_eq!(snap_to_char_boundary(s, 6), 6);
        // Middle of "世" (bytes 6,7,8) should snap to 9
        assert_eq!(snap_to_char_boundary(s, 7), 9);
        assert_eq!(snap_to_char_boundary(s, 8), 9);
        // Past end should clamp
        assert_eq!(snap_to_char_boundary(s, 100), 12);
    }

    #[tokio::test]
    async fn test_context_info() {
        let ctx = Arc::new("Line one\nLine two\nLine three".to_string());
        let tool = ContextInfoTool { context: ctx };
        let result = tool.execute(json!({})).await.unwrap();
        assert_eq!(result["line_count"], 3);
        assert_eq!(result["length_chars"], 28);
        assert!(result["estimated_tokens"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn test_context_peek_basic() {
        let ctx = Arc::new("Hello, world! This is a test.".to_string());
        let tool = ContextPeekTool { context: ctx };
        let result = tool.execute(json!({"start": 0, "end": 13})).await.unwrap();
        assert_eq!(result["text"], "Hello, world!");
        assert_eq!(result["truncated"], false);
    }

    #[tokio::test]
    async fn test_context_peek_clamps_bounds() {
        let ctx = Arc::new("short".to_string());
        let tool = ContextPeekTool { context: ctx };
        let result = tool
            .execute(json!({"start": 0, "end": 99999}))
            .await
            .unwrap();
        assert_eq!(result["text"], "short");
    }

    #[tokio::test]
    async fn test_context_search_finds_matches() {
        let ctx = Arc::new(
            "The temperature in Tokyo is 25C.\n\
             Rain expected in London.\n\
             Tokyo will be sunny tomorrow."
                .to_string(),
        );
        let tool = ContextSearchTool { context: ctx };
        let result = tool
            .execute(json!({"pattern": "tokyo", "max_results": 10}))
            .await
            .unwrap();
        assert_eq!(result["total_matches"], 2);
        let matches = result["matches"].as_array().unwrap();
        assert_eq!(matches[0]["line_number"], 1);
        assert_eq!(matches[1]["line_number"], 3);
    }

    #[tokio::test]
    async fn test_context_search_invalid_regex() {
        let ctx = Arc::new("test".to_string());
        let tool = ContextSearchTool { context: ctx };
        let result = tool.execute(json!({"pattern": "[invalid"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_context_search_respects_max_results() {
        let ctx = Arc::new("match\nmatch\nmatch\nmatch\nmatch".to_string());
        let tool = ContextSearchTool { context: ctx };
        let result = tool
            .execute(json!({"pattern": "match", "max_results": 2}))
            .await
            .unwrap();
        assert_eq!(result["total_matches"], 2);
    }
}
