//! REPL-style tools for recursive context exploration.
//!
//! Instead of dumping the full context into the prompt, we store it
//! externally and give the model tools to peek, search, and delegate
//! sub-queries — mirroring the foundational RLM paper's Python REPL
//! approach (Zhang, Kraska, Khattab — arXiv 2512.24601).
//!
//! Two binding modes are supported:
//!
//! - **Fixed**: a pre-built `Arc<String>`, used by [`RecursiveExecutor`]
//!   for a single call against a known blob.
//! - **Store**: an [`Arc<ContextStore>`] keyed by the active
//!   conversation's id. This is what [`context_tools`] returns — the
//!   tools resolve their context lazily from the
//!   `SESSION_TOOL_CONTEXT::conversation_id` task-local on every call,
//!   so the same registered tool instances work for every session.
//!
//! [`RecursiveExecutor`]: super::recursive_call::RecursiveExecutor

use std::sync::Arc;

use async_trait::async_trait;
use regex::Regex;
use rustykrab_core::active_tools::SESSION_TOOL_CONTEXT;
use rustykrab_core::model::ModelProvider;
use rustykrab_core::orchestration::OrchestrationConfig;
use rustykrab_core::tool::Tool;
use rustykrab_core::types::ToolSchema;
use serde_json::{json, Value};
use tokio::sync::Semaphore;

use super::context_manager::estimate_tokens;
use super::context_store::{ContextStore, MAX_CONTEXT_BYTES};

/// Maximum characters returned by a single `context_peek` call.
const MAX_PEEK_CHARS: usize = 50_000;

/// How a [`ContextInfoTool`] / [`ContextPeekTool`] / [`ContextSearchTool`]
/// finds the blob it should operate on.
#[derive(Clone)]
enum ContextSource {
    /// Use the same fixed blob for every call (the
    /// [`RecursiveExecutor`] path).
    Fixed(Arc<String>),
    /// Look the blob up in a [`ContextStore`] by the active
    /// conversation id read from `SESSION_TOOL_CONTEXT`.
    Store(Arc<ContextStore>),
}

impl ContextSource {
    fn resolve(&self) -> Option<Arc<String>> {
        match self {
            ContextSource::Fixed(s) => Some(s.clone()),
            ContextSource::Store(store) => SESSION_TOOL_CONTEXT
                .try_with(|ctx| store.get(ctx.conversation_id))
                .ok()
                .flatten(),
        }
    }
}

fn no_context_error() -> rustykrab_core::Error {
    rustykrab_core::Error::ToolExecution(
        "no context bound for this conversation; call `context_set` first".into(),
    )
}

/// Build the set of REPL tools for a given recursion level. Used by
/// [`RecursiveExecutor`] with a fixed context blob.
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
    let source = ContextSource::Fixed(context.clone());
    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(ContextInfoTool {
            source: source.clone(),
        }),
        Arc::new(ContextPeekTool {
            source: source.clone(),
        }),
        Arc::new(ContextSearchTool { source }),
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

/// Build the always-on context tools backed by a per-conversation
/// [`ContextStore`]. Returns four tools: `context_set`, `context_info`,
/// `context_peek`, `context_search`. `sub_query` is intentionally not
/// included in this builder — wiring recursive sub-calls into the main
/// agent runner is a separate change.
pub fn context_tools(store: Arc<ContextStore>) -> Vec<Arc<dyn Tool>> {
    let source = ContextSource::Store(store.clone());
    vec![
        Arc::new(ContextSetTool { store }),
        Arc::new(ContextInfoTool {
            source: source.clone(),
        }),
        Arc::new(ContextPeekTool {
            source: source.clone(),
        }),
        Arc::new(ContextSearchTool { source }),
    ]
}

// ── context_set ─────────────────────────────────────────────────────

struct ContextSetTool {
    store: Arc<ContextStore>,
}

#[async_trait]
impl Tool for ContextSetTool {
    fn name(&self) -> &str {
        "context_set"
    }

    fn description(&self) -> &str {
        "Stash a large blob of text outside the prompt for the current \
         conversation. After calling this you can use `context_info`, \
         `context_peek`, and `context_search` to explore the blob \
         without paying the token cost of having it in every turn. \
         Replaces any previously stashed context for this conversation."
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
                        "description": "The blob to stash. Maximum 4 MiB."
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

        if text.len() > MAX_CONTEXT_BYTES {
            return Err(rustykrab_core::Error::ToolExecution(
                format!(
                    "context too large: {} bytes exceeds {} byte limit",
                    text.len(),
                    MAX_CONTEXT_BYTES
                )
                .into(),
            ));
        }

        let conv_id = SESSION_TOOL_CONTEXT
            .try_with(|ctx| ctx.conversation_id)
            .map_err(|_| {
                rustykrab_core::Error::ToolExecution(
                    "context_set called outside a session context".into(),
                )
            })?;

        let bytes = self.store.set(conv_id, text.to_string());
        Ok(json!({
            "stored_bytes": bytes,
            "estimated_tokens": estimate_tokens(text),
        }))
    }
}

// ── context_info ────────────────────────────────────────────────────

struct ContextInfoTool {
    source: ContextSource,
}

#[async_trait]
impl Tool for ContextInfoTool {
    fn name(&self) -> &str {
        "context_info"
    }

    fn description(&self) -> &str {
        "Get metadata about the context variable: byte length, \
         character count, estimated token count, line count, and a \
         short preview of the first 500 characters."
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
        let context = self.source.resolve().ok_or_else(no_context_error)?;
        let length_bytes = context.len();
        let length_chars = context.chars().count();
        let estimated_tokens = estimate_tokens(&context);
        let line_count = context.lines().count();
        let preview_end = context
            .char_indices()
            .take_while(|(i, _)| *i < 500)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(length_bytes.min(500));
        let preview = &context[..preview_end];

        Ok(json!({
            "length_bytes": length_bytes,
            "length_chars": length_chars,
            "estimated_tokens": estimated_tokens,
            "line_count": line_count,
            "preview": preview
        }))
    }
}

// ── context_peek ────────────────────────────────────────────────────

struct ContextPeekTool {
    source: ContextSource,
}

#[async_trait]
impl Tool for ContextPeekTool {
    fn name(&self) -> &str {
        "context_peek"
    }

    fn description(&self) -> &str {
        "View a slice of the context by byte offset. Returns \
         context[start..end]. Offsets are snapped to valid UTF-8 \
         boundaries. Clamped to context bounds and a safety limit \
         of 50 000 bytes per call."
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
                        "description": "Start byte offset (0-indexed, inclusive). Snapped to nearest UTF-8 boundary."
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
        let context = self.source.resolve().ok_or_else(no_context_error)?;
        let start = args["start"].as_u64().unwrap_or(0) as usize;
        let end = args["end"].as_u64().unwrap_or(0) as usize;

        let ctx_len = context.len();
        let start = start.min(ctx_len);
        let end = end.min(ctx_len).max(start);

        // Snap to char boundaries.
        let start = snap_to_char_boundary(&context, start);
        let end = snap_to_char_boundary(&context, end);

        // Enforce safety limit.
        let effective_end = end.min(start + MAX_PEEK_CHARS);
        let effective_end = snap_to_char_boundary(&context, effective_end);

        let slice = &context[start..effective_end];
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
    source: ContextSource,
}

#[async_trait]
impl Tool for ContextSearchTool {
    fn name(&self) -> &str {
        "context_search"
    }

    fn description(&self) -> &str {
        "Search the context using a regex pattern. Returns matching \
         lines with their line numbers and byte offsets. Use the \
         byte_offset values with context_peek or sub_query."
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
        let context = self.source.resolve().ok_or_else(no_context_error)?;
        let pattern = args["pattern"]
            .as_str()
            .ok_or_else(|| rustykrab_core::Error::ToolExecution("missing pattern".into()))?;
        let max_results = args["max_results"].as_u64().unwrap_or(20) as usize;

        let re = Regex::new(&format!("(?i){pattern}")).map_err(|e| {
            rustykrab_core::Error::ToolExecution(format!("invalid regex: {e}").into())
        })?;

        let mut matches = Vec::new();
        let mut total_count = 0usize;
        // Track byte offset by pointer arithmetic against the original
        // string so we handle both \n and \r\n correctly.
        let ctx_start = context.as_ptr() as usize;

        for (line_num, line) in context.lines().enumerate() {
            if re.is_match(line) {
                let byte_offset = line.as_ptr() as usize - ctx_start;
                total_count += 1;
                if matches.len() < max_results {
                    matches.push(json!({
                        "line_number": line_num + 1,
                        "byte_offset": byte_offset,
                        "text": if line.len() > 200 {
                            format!("{}...", &line[..line.floor_char_boundary(200)])
                        } else {
                            line.to_string()
                        }
                    }));
                }
            }
        }

        Ok(json!({
            "total_matches": total_count,
            "returned_matches": matches.len(),
            "truncated": total_count > matches.len(),
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
                        "description": "Start byte offset of the context slice (inclusive). Snapped to nearest UTF-8 boundary."
                    },
                    "end": {
                        "type": "integer",
                        "description": "End byte offset of the context slice (exclusive). Snapped to nearest UTF-8 boundary."
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

        // NOTE: We do NOT acquire the semaphore here. The semaphore
        // gates individual provider.chat() calls inside
        // execute_repl_call's tool-use loop. Holding a permit across
        // the entire recursive subtree would deadlock at
        // depth >= permit count.
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

    use rustykrab_core::active_tools::{ActiveToolsRegistry, SessionToolContext};
    use rustykrab_core::CapabilitySet;
    use uuid::Uuid;

    fn fixed(text: &str) -> ContextSource {
        ContextSource::Fixed(Arc::new(text.to_string()))
    }

    #[tokio::test]
    async fn test_context_info() {
        let tool = ContextInfoTool {
            source: fixed("Line one\nLine two\nLine three"),
        };
        let result = tool.execute(json!({})).await.unwrap();
        assert_eq!(result["line_count"], 3);
        assert_eq!(result["length_bytes"], 28);
        assert_eq!(result["length_chars"], 28); // ASCII: bytes == chars
        assert!(result["estimated_tokens"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn test_context_peek_basic() {
        let tool = ContextPeekTool {
            source: fixed("Hello, world! This is a test."),
        };
        let result = tool.execute(json!({"start": 0, "end": 13})).await.unwrap();
        assert_eq!(result["text"], "Hello, world!");
        assert_eq!(result["truncated"], false);
    }

    #[tokio::test]
    async fn test_context_peek_clamps_bounds() {
        let tool = ContextPeekTool {
            source: fixed("short"),
        };
        let result = tool
            .execute(json!({"start": 0, "end": 99999}))
            .await
            .unwrap();
        assert_eq!(result["text"], "short");
    }

    #[tokio::test]
    async fn test_context_search_finds_matches() {
        let tool = ContextSearchTool {
            source: fixed(
                "The temperature in Tokyo is 25C.\n\
                 Rain expected in London.\n\
                 Tokyo will be sunny tomorrow.",
            ),
        };
        let result = tool
            .execute(json!({"pattern": "tokyo", "max_results": 10}))
            .await
            .unwrap();
        assert_eq!(result["total_matches"], 2);
        assert_eq!(result["returned_matches"], 2);
        assert_eq!(result["truncated"], false);
        let matches = result["matches"].as_array().unwrap();
        assert_eq!(matches[0]["line_number"], 1);
        assert!(matches[0].get("byte_offset").is_some());
        assert_eq!(matches[1]["line_number"], 3);
    }

    #[tokio::test]
    async fn test_context_search_invalid_regex() {
        let tool = ContextSearchTool {
            source: fixed("test"),
        };
        let result = tool.execute(json!({"pattern": "[invalid"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_context_search_respects_max_results() {
        let tool = ContextSearchTool {
            source: fixed("match\nmatch\nmatch\nmatch\nmatch"),
        };
        let result = tool
            .execute(json!({"pattern": "match", "max_results": 2}))
            .await
            .unwrap();
        // total_matches reports the true count, not the capped count
        assert_eq!(result["total_matches"], 5);
        assert_eq!(result["returned_matches"], 2);
        assert_eq!(result["truncated"], true);
        assert_eq!(result["matches"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn test_context_search_crlf_offsets() {
        // \r\n line endings — byte offsets must account for 2-byte separators
        let tool = ContextSearchTool {
            source: fixed("first\r\nsecond\r\nthird"),
        };
        let result = tool.execute(json!({"pattern": "third"})).await.unwrap();
        let matches = result["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1);
        // "first\r\n" = 7 bytes, "second\r\n" = 8 bytes → "third" starts at byte 15
        assert_eq!(matches[0]["byte_offset"], 15);
    }

    fn session_ctx(conv_id: Uuid) -> SessionToolContext {
        SessionToolContext {
            conversation_id: conv_id,
            capabilities: Arc::new(CapabilitySet::default_safe()),
            all_tools: Arc::new(Vec::new()),
            active_tools: Arc::new(ActiveToolsRegistry::new()),
        }
    }

    #[tokio::test]
    async fn store_source_resolves_via_session_context() {
        let store = Arc::new(ContextStore::new());
        let conv_id = Uuid::new_v4();
        store.set(conv_id, "stored text".into());

        let tool = ContextInfoTool {
            source: ContextSource::Store(store),
        };
        let result = SESSION_TOOL_CONTEXT
            .scope(session_ctx(conv_id), tool.execute(json!({})))
            .await
            .unwrap();
        assert_eq!(result["length_bytes"], 11);
    }

    #[tokio::test]
    async fn store_source_errors_when_no_context_bound() {
        let store = Arc::new(ContextStore::new());
        let conv_id = Uuid::new_v4(); // never set
        let tool = ContextInfoTool {
            source: ContextSource::Store(store),
        };
        let result = SESSION_TOOL_CONTEXT
            .scope(session_ctx(conv_id), tool.execute(json!({})))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no context bound"));
    }

    #[tokio::test]
    async fn context_set_stashes_into_store() {
        let store = Arc::new(ContextStore::new());
        let conv_id = Uuid::new_v4();
        let tool = ContextSetTool {
            store: store.clone(),
        };

        let result = SESSION_TOOL_CONTEXT
            .scope(
                session_ctx(conv_id),
                tool.execute(json!({"text": "hello world"})),
            )
            .await
            .unwrap();
        assert_eq!(result["stored_bytes"], 11);
        assert_eq!(
            store.get(conv_id).as_deref().map(String::as_str),
            Some("hello world")
        );
    }

    #[tokio::test]
    async fn context_set_rejects_oversize_blob() {
        let store = Arc::new(ContextStore::new());
        let conv_id = Uuid::new_v4();
        let tool = ContextSetTool { store };

        let big = "x".repeat(MAX_CONTEXT_BYTES + 1);
        let err = SESSION_TOOL_CONTEXT
            .scope(session_ctx(conv_id), tool.execute(json!({"text": big})))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("context too large"));
    }

    #[tokio::test]
    async fn context_set_errors_outside_session_scope() {
        let store = Arc::new(ContextStore::new());
        let tool = ContextSetTool { store };
        let err = tool.execute(json!({"text": "x"})).await.unwrap_err();
        assert!(err.to_string().contains("outside a session context"));
    }
}
