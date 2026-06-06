//! `recall_*` tools — REPL-style access to compaction-displaced history.
//!
//! When [`runner::AgentRunner::compact_history`](crate::AgentRunner)
//! summarises a long conversation, the original messages drop out of
//! the prompt.  The summary keeps the active prompt small, but specific
//! detail (numbers, file paths, intermediate tool outputs) can be lost
//! in compression.
//!
//! These tools give the agent a way to recover that detail without
//! re-reading everything.  The displaced text lives in the per-session
//! [`RecallStore`] (see `rustykrab-core::recall`); these tools read
//! from it via [`with_session_context`].
//!
//! They mirror the `context_*` REPL tools used inside
//! [`crate::rlm::RecursiveExecutor`], which follow the foundational RLM
//! paper's pattern (Zhang, Kraska, Khattab — arXiv 2512.24601): keep
//! the long context outside the prompt, and let the model navigate it.

use std::sync::Arc;

use async_trait::async_trait;
use regex::Regex;
use rustykrab_core::active_tools::with_session_context;
use rustykrab_core::model::ModelProvider;
use rustykrab_core::orchestration::OrchestrationConfig;
use rustykrab_core::tool::Tool;
use rustykrab_core::types::ToolSchema;
use serde_json::{json, Value};

use crate::rlm::estimate_tokens;
use crate::rlm::RecursiveExecutor;

/// Maximum characters returned by a single `recall_peek` call.  Mirrors
/// `MAX_PEEK_CHARS` in `rlm::repl_tools` so a single peek cannot blow up
/// the prompt on its own.
const MAX_PEEK_CHARS: usize = 50_000;

/// Maximum bytes a single `recall_append` call may stash. Bounds memory
/// growth from a model that tries to dump a huge blob in one shot;
/// larger payloads should be appended in chunks.
const MAX_APPEND_BYTES: usize = 4 * 1024 * 1024;

/// Build the recall tools.  All take no per-call construction arguments —
/// they resolve the active conversation's archive at `execute()` time via
/// [`with_session_context`], so the same instances can be registered
/// globally and shared across conversations.
pub fn recall_tools(
    provider: Arc<dyn ModelProvider>,
    orchestration: OrchestrationConfig,
) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(RecallAppendTool),
        Arc::new(RecallInfoTool),
        Arc::new(RecallPeekTool),
        Arc::new(RecallSearchTool),
        Arc::new(RecallSubQueryTool {
            provider,
            orchestration,
        }),
    ]
}

/// Fetch the current session's archive, or `None` if no archive exists
/// for this conversation (e.g. compaction has not fired yet, or this
/// tool was invoked outside a runner scope).
fn current_archive() -> Option<Arc<String>> {
    with_session_context(|ctx| ctx.recall.get(ctx.conversation_id)).flatten()
}

/// Standard "no archive" payload — returned when the agent calls a
/// recall tool before any history has been displaced.  Telling the
/// model `empty: true` is friendlier than failing, because compaction
/// is non-deterministic from the model's point of view.
fn empty_archive_response() -> Value {
    json!({
        "empty": true,
        "note": "no compacted history is available for this conversation yet",
    })
}

// ── recall_append ───────────────────────────────────────────────────

struct RecallAppendTool;

#[async_trait]
impl Tool for RecallAppendTool {
    fn name(&self) -> &str {
        "recall_append"
    }

    fn description(&self) -> &str {
        "Stash a blob of text into this conversation's recall archive so \
         you can come back to it later via `recall_info`, `recall_peek`, \
         and `recall_search` without paying the token cost of carrying it \
         in every turn. The archive is append-only — new text is joined \
         with a blank-line separator to whatever is already there. Useful \
         for large documents, long tool outputs, or chunks of context the \
         user pasted that you only need to consult occasionally."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "The text to append to the archive. Maximum 4 MiB per call."
                    }
                },
                "required": ["text"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> rustykrab_core::Result<Value> {
        let text = args["text"]
            .as_str()
            .ok_or_else(|| rustykrab_core::Error::ToolExecution("missing text".into()))?;

        if text.len() > MAX_APPEND_BYTES {
            return Err(rustykrab_core::Error::ToolExecution(
                format!(
                    "text too large: {} bytes exceeds {} byte limit; split into smaller appends",
                    text.len(),
                    MAX_APPEND_BYTES
                )
                .into(),
            ));
        }

        let result = with_session_context(|ctx| {
            ctx.recall.append(ctx.conversation_id, text);
            ctx.recall
                .get(ctx.conversation_id)
                .map(|a| a.len())
                .unwrap_or(0)
        });

        match result {
            Some(archive_bytes) => Ok(json!({
                "appended_bytes": text.len(),
                "archive_bytes": archive_bytes,
                "estimated_tokens": estimate_tokens(text),
            })),
            None => Err(rustykrab_core::Error::ToolExecution(
                "recall_append called outside a session context".into(),
            )),
        }
    }
}

// ── recall_info ─────────────────────────────────────────────────────

struct RecallInfoTool;

#[async_trait]
impl Tool for RecallInfoTool {
    fn name(&self) -> &str {
        "recall_info"
    }

    fn description(&self) -> &str {
        "Get metadata about the compacted-history archive for this \
         conversation: byte length, character count, estimated tokens, \
         line count, and a 500-character preview. Use this first to see \
         whether earlier detail is available before recalling specifics."
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
        let Some(archive) = current_archive() else {
            return Ok(empty_archive_response());
        };
        let length_bytes = archive.len();
        let length_chars = archive.chars().count();
        let estimated_tokens = estimate_tokens(&archive);
        let line_count = archive.lines().count();
        let preview_end = archive
            .char_indices()
            .take_while(|(i, _)| *i < 500)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(length_bytes.min(500));
        let preview = &archive[..preview_end];
        Ok(json!({
            "empty": false,
            "length_bytes": length_bytes,
            "length_chars": length_chars,
            "estimated_tokens": estimated_tokens,
            "line_count": line_count,
            "preview": preview,
        }))
    }
}

// ── recall_peek ─────────────────────────────────────────────────────

struct RecallPeekTool;

#[async_trait]
impl Tool for RecallPeekTool {
    fn name(&self) -> &str {
        "recall_peek"
    }

    fn description(&self) -> &str {
        "View a slice of the compacted-history archive by byte offset. \
         Returns archive[start..end]. Offsets are snapped to UTF-8 \
         character boundaries and clamped to the archive bounds. A \
         single peek is capped at 50,000 bytes."
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
                        "description": "Start byte offset (inclusive). Snapped to nearest UTF-8 boundary."
                    },
                    "end": {
                        "type": "integer",
                        "description": "End byte offset (exclusive). Snapped to nearest UTF-8 boundary."
                    }
                },
                "required": ["start", "end"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> rustykrab_core::Result<Value> {
        let Some(archive) = current_archive() else {
            return Ok(empty_archive_response());
        };
        let start = args["start"].as_u64().unwrap_or(0) as usize;
        let end = args["end"].as_u64().unwrap_or(0) as usize;
        let len = archive.len();
        let start = snap_to_char_boundary(&archive, start.min(len));
        let end = snap_to_char_boundary(&archive, end.min(len).max(start));
        let effective_end = end.min(start + MAX_PEEK_CHARS);
        let effective_end = snap_to_char_boundary(&archive, effective_end);
        let slice = &archive[start..effective_end];
        Ok(json!({
            "empty": false,
            "text": slice,
            "start": start,
            "end": effective_end,
            "truncated": effective_end < end,
        }))
    }
}

// ── recall_search ───────────────────────────────────────────────────

struct RecallSearchTool;

#[async_trait]
impl Tool for RecallSearchTool {
    fn name(&self) -> &str {
        "recall_search"
    }

    fn description(&self) -> &str {
        "Regex-search the compacted-history archive (case-insensitive). \
         Returns matching lines with line numbers and byte offsets — \
         use the byte offsets with recall_peek or recall_sub_query to \
         retrieve the surrounding text."
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
                        "description": "Regex pattern (case-insensitive)."
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Maximum matches to return (default 20)."
                    }
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> rustykrab_core::Result<Value> {
        let Some(archive) = current_archive() else {
            return Ok(empty_archive_response());
        };
        let pattern = args["pattern"]
            .as_str()
            .ok_or_else(|| rustykrab_core::Error::ToolExecution("missing pattern".into()))?;
        let max_results = args["max_results"].as_u64().unwrap_or(20) as usize;
        let re = Regex::new(&format!("(?i){pattern}")).map_err(|e| {
            rustykrab_core::Error::ToolExecution(format!("invalid regex: {e}").into())
        })?;
        let mut matches = Vec::new();
        let mut total_count = 0usize;
        let archive_start = archive.as_ptr() as usize;
        for (line_num, line) in archive.lines().enumerate() {
            if re.is_match(line) {
                let byte_offset = line.as_ptr() as usize - archive_start;
                total_count += 1;
                if matches.len() < max_results {
                    let text = if line.len() > 200 {
                        let mut end = 200;
                        while end > 0 && !line.is_char_boundary(end) {
                            end -= 1;
                        }
                        format!("{}...", &line[..end])
                    } else {
                        line.to_string()
                    };
                    matches.push(json!({
                        "line_number": line_num + 1,
                        "byte_offset": byte_offset,
                        "text": text,
                    }));
                }
            }
        }
        Ok(json!({
            "empty": false,
            "total_matches": total_count,
            "returned_matches": matches.len(),
            "truncated": total_count > matches.len(),
            "matches": matches,
        }))
    }
}

// ── recall_sub_query ────────────────────────────────────────────────

struct RecallSubQueryTool {
    provider: Arc<dyn ModelProvider>,
    orchestration: OrchestrationConfig,
}

#[async_trait]
impl Tool for RecallSubQueryTool {
    fn name(&self) -> &str {
        "recall_sub_query"
    }

    fn description(&self) -> &str {
        "Launch a focused sub-LLM call against a slice of the \
         compacted-history archive. The sub-call gets its own REPL \
         tools (context_info / context_peek / context_search) over \
         the slice. Use this to delegate analysis of a section you \
         identified via recall_search — prefer one larger slice over \
         many tiny ones."
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
                        "description": "Question for the sub-call to answer."
                    },
                    "start": {
                        "type": "integer",
                        "description": "Start byte offset of the slice (inclusive). Snapped to a UTF-8 boundary."
                    },
                    "end": {
                        "type": "integer",
                        "description": "End byte offset of the slice (exclusive). Snapped to a UTF-8 boundary."
                    }
                },
                "required": ["question", "start", "end"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> rustykrab_core::Result<Value> {
        let Some(archive) = current_archive() else {
            return Ok(empty_archive_response());
        };
        let question = args["question"]
            .as_str()
            .ok_or_else(|| rustykrab_core::Error::ToolExecution("missing question".into()))?;
        let start = args["start"].as_u64().unwrap_or(0) as usize;
        let end = args["end"].as_u64().unwrap_or(0) as usize;
        let len = archive.len();
        let start = snap_to_char_boundary(&archive, start.min(len));
        let end = snap_to_char_boundary(&archive, end.min(len).max(start));
        let slice = archive[start..end].to_string();
        let executor = RecursiveExecutor::new(self.provider.clone(), self.orchestration.clone());
        let answer = executor.execute(question, Some(&slice)).await?;
        Ok(json!({
            "empty": false,
            "answer": answer,
            "start": start,
            "end": end,
        }))
    }
}

// ── helpers ─────────────────────────────────────────────────────────

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

    use rustykrab_core::active_tools::{ActiveToolsRegistry, SessionToolContext};
    use rustykrab_core::capability::CapabilitySet;
    use rustykrab_core::recall::RecallStore;
    use rustykrab_core::SESSION_TOOL_CONTEXT;
    use uuid::Uuid;

    fn ctx_with_archive(text: &str) -> (SessionToolContext, Uuid) {
        let conv = Uuid::new_v4();
        let recall = Arc::new(RecallStore::new());
        recall.append(conv, text);
        let ctx = SessionToolContext {
            conversation_id: conv,
            capabilities: Arc::new(CapabilitySet::none()),
            all_tools: Arc::new(Vec::new()),
            active_tools: Arc::new(ActiveToolsRegistry::new()),
            recall,
            todos: Arc::new(rustykrab_core::TodoStore::new()),
        };
        (ctx, conv)
    }

    fn empty_ctx() -> SessionToolContext {
        SessionToolContext {
            conversation_id: Uuid::new_v4(),
            capabilities: Arc::new(CapabilitySet::none()),
            all_tools: Arc::new(Vec::new()),
            active_tools: Arc::new(ActiveToolsRegistry::new()),
            recall: Arc::new(RecallStore::new()),
            todos: Arc::new(rustykrab_core::TodoStore::new()),
        }
    }

    #[tokio::test]
    async fn info_reports_archive_metadata() {
        let (ctx, _) = ctx_with_archive("Line one\nLine two\nLine three");
        let result = SESSION_TOOL_CONTEXT
            .scope(ctx, async { RecallInfoTool.execute(json!({})).await })
            .await
            .unwrap();
        assert_eq!(result["empty"], false);
        assert_eq!(result["line_count"], 3);
        assert_eq!(result["length_bytes"], 28);
    }

    #[tokio::test]
    async fn info_returns_empty_when_no_archive() {
        let ctx = empty_ctx();
        let result = SESSION_TOOL_CONTEXT
            .scope(ctx, async { RecallInfoTool.execute(json!({})).await })
            .await
            .unwrap();
        assert_eq!(result["empty"], true);
    }

    #[tokio::test]
    async fn peek_returns_slice() {
        let (ctx, _) = ctx_with_archive("Hello, world! This is a test.");
        let result = SESSION_TOOL_CONTEXT
            .scope(ctx, async {
                RecallPeekTool.execute(json!({"start": 0, "end": 13})).await
            })
            .await
            .unwrap();
        assert_eq!(result["text"], "Hello, world!");
        assert_eq!(result["truncated"], false);
    }

    #[tokio::test]
    async fn peek_clamps_bounds() {
        let (ctx, _) = ctx_with_archive("short");
        let result = SESSION_TOOL_CONTEXT
            .scope(ctx, async {
                RecallPeekTool
                    .execute(json!({"start": 0, "end": 99999}))
                    .await
            })
            .await
            .unwrap();
        assert_eq!(result["text"], "short");
    }

    #[tokio::test]
    async fn search_finds_matches_with_offsets() {
        let (ctx, _) = ctx_with_archive(
            "The temperature in Tokyo is 25C.\nRain expected in London.\nTokyo will be sunny tomorrow.",
        );
        let result = SESSION_TOOL_CONTEXT
            .scope(ctx, async {
                RecallSearchTool.execute(json!({"pattern": "tokyo"})).await
            })
            .await
            .unwrap();
        assert_eq!(result["total_matches"], 2);
        let matches = result["matches"].as_array().unwrap();
        assert_eq!(matches[0]["line_number"], 1);
        assert_eq!(matches[1]["line_number"], 3);
    }

    #[tokio::test]
    async fn search_returns_empty_when_no_archive() {
        let ctx = empty_ctx();
        let result = SESSION_TOOL_CONTEXT
            .scope(ctx, async {
                RecallSearchTool
                    .execute(json!({"pattern": "anything"}))
                    .await
            })
            .await
            .unwrap();
        assert_eq!(result["empty"], true);
    }

    #[tokio::test]
    async fn search_invalid_regex_errors() {
        let (ctx, _) = ctx_with_archive("text");
        let result = SESSION_TOOL_CONTEXT
            .scope(ctx, async {
                RecallSearchTool
                    .execute(json!({"pattern": "[invalid"}))
                    .await
            })
            .await;
        assert!(result.is_err());
    }

    fn empty_ctx_with_store() -> (SessionToolContext, Uuid, Arc<RecallStore>) {
        let conv = Uuid::new_v4();
        let recall = Arc::new(RecallStore::new());
        let ctx = SessionToolContext {
            conversation_id: conv,
            capabilities: Arc::new(CapabilitySet::none()),
            all_tools: Arc::new(Vec::new()),
            active_tools: Arc::new(ActiveToolsRegistry::new()),
            recall: recall.clone(),
            todos: Arc::new(rustykrab_core::TodoStore::new()),
        };
        (ctx, conv, recall)
    }

    #[tokio::test]
    async fn append_stashes_into_archive() {
        let (ctx, conv, store) = empty_ctx_with_store();
        let result = SESSION_TOOL_CONTEXT
            .scope(ctx, async {
                RecallAppendTool
                    .execute(json!({"text": "hello world"}))
                    .await
            })
            .await
            .unwrap();
        assert_eq!(result["appended_bytes"], 11);
        assert_eq!(result["archive_bytes"], 11);
        assert_eq!(
            store.get(conv).as_deref().map(String::as_str),
            Some("hello world")
        );
    }

    #[tokio::test]
    async fn append_is_readable_via_info_and_peek() {
        let (ctx, _, _) = empty_ctx_with_store();
        let result = SESSION_TOOL_CONTEXT
            .scope(ctx, async {
                RecallAppendTool
                    .execute(json!({"text": "first chunk"}))
                    .await?;
                let info = RecallInfoTool.execute(json!({})).await?;
                let peek = RecallPeekTool
                    .execute(json!({"start": 0, "end": 5}))
                    .await?;
                Ok::<_, rustykrab_core::Error>(json!({"info": info, "peek": peek}))
            })
            .await
            .unwrap();
        assert_eq!(result["info"]["empty"], false);
        assert_eq!(result["info"]["length_bytes"], 11);
        assert_eq!(result["peek"]["text"], "first");
    }

    #[tokio::test]
    async fn append_joins_with_blank_line_separator() {
        let (ctx, conv, store) = empty_ctx_with_store();
        SESSION_TOOL_CONTEXT
            .scope(ctx, async {
                RecallAppendTool
                    .execute(json!({"text": "first"}))
                    .await
                    .unwrap();
                RecallAppendTool
                    .execute(json!({"text": "second"}))
                    .await
                    .unwrap();
            })
            .await;
        assert_eq!(
            store.get(conv).as_deref().map(String::as_str),
            Some("first\n\nsecond")
        );
    }

    #[tokio::test]
    async fn append_rejects_oversize_payload() {
        let (ctx, _, _) = empty_ctx_with_store();
        let big = "x".repeat(MAX_APPEND_BYTES + 1);
        let err = SESSION_TOOL_CONTEXT
            .scope(ctx, async {
                RecallAppendTool.execute(json!({"text": big})).await
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("text too large"));
    }

    #[tokio::test]
    async fn append_errors_outside_session_scope() {
        let err = RecallAppendTool
            .execute(json!({"text": "x"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("outside a session context"));
    }
}
