//! `todo_*` tools — the agent's intra-loop planning scratchpad.
//!
//! A structured todo list is the standard remedy for goal drift over long
//! task horizons: the agent writes the plan down, then re-reads and rewrites
//! it as it works, so the objective stays in view even after the surrounding
//! context has churned. Unlike the `recall_*` tools (which recover *facts*
//! displaced by compaction), these maintain the agent's *intentions*.
//!
//! State lives in the per-conversation [`TodoStore`](rustykrab_core::TodoStore)
//! on the runner's [`SessionToolContext`]; these tools resolve it at
//! `execute()` time via [`with_session_context`], so the same instances are
//! registered globally and shared across conversations. The runner re-emits
//! the rendered list verbatim across a compaction
//! ([`compact_history`](crate::AgentRunner)) so the plan survives the one
//! event most likely to lose it.

use std::sync::Arc;

use async_trait::async_trait;
use rustykrab_core::active_tools::with_session_context;
use rustykrab_core::tool::Tool;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Error, TodoItem, TodoStatus};
use serde_json::{json, Value};

/// Maximum number of items a single todo list may hold. Keeps the rendered
/// checklist — which is re-emitted on every compaction — bounded.
const MAX_TODO_ITEMS: usize = 100;

/// Build the todo tools. Both are stateless structs that resolve the active
/// conversation's list at `execute()` time, so they can be registered once
/// and shared across conversations.
pub fn todo_tools() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(TodoWriteTool), Arc::new(TodoReadTool)]
}

/// Render the JSON payload returned to the model after a read or write:
/// the markdown checklist plus per-status counts so progress is legible.
fn list_payload(items: &[TodoItem]) -> Value {
    let pending = items
        .iter()
        .filter(|i| i.status == TodoStatus::Pending)
        .count();
    let in_progress = items
        .iter()
        .filter(|i| i.status == TodoStatus::InProgress)
        .count();
    let completed = items
        .iter()
        .filter(|i| i.status == TodoStatus::Completed)
        .count();
    json!({
        "todos": items
            .iter()
            .map(|i| json!({ "content": i.content, "status": i.status.as_str() }))
            .collect::<Vec<_>>(),
        "rendered": rustykrab_core::render_todos(items),
        "counts": {
            "total": items.len(),
            "pending": pending,
            "in_progress": in_progress,
            "completed": completed,
        },
    })
}

// ── todo_write ──────────────────────────────────────────────────────────

struct TodoWriteTool;

#[async_trait]
impl Tool for TodoWriteTool {
    fn name(&self) -> &str {
        "todo_write"
    }

    fn description(&self) -> &str {
        "Record or update your task list for this conversation. Pass the \
         WHOLE list every time — it replaces the previous one, so include \
         already-completed items (marked completed) alongside what's left. \
         Each item is {content, status} where status is one of \"pending\", \
         \"in_progress\", or \"completed\". Keep exactly one item \
         \"in_progress\" to mark your current focus. Use this for any task \
         with more than a couple of steps: write the plan up front, then \
         flip statuses as you go. The list is preserved across context \
         compaction, so it's your durable anchor on long tasks. Send an \
         empty list to clear it."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "description": "The full task list, in order. Replaces any previous list.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": {
                                    "type": "string",
                                    "description": "The step, as a short imperative phrase."
                                },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"],
                                    "description": "Lifecycle state of this step."
                                }
                            },
                            "required": ["content", "status"]
                        }
                    }
                },
                "required": ["todos"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> rustykrab_core::Result<Value> {
        let raw = args
            .get("todos")
            .and_then(|v| v.as_array())
            .ok_or_else(|| Error::ToolExecution("missing `todos` array".into()))?;

        if raw.len() > MAX_TODO_ITEMS {
            return Err(Error::ToolExecution(
                format!(
                    "too many items: {} exceeds the {MAX_TODO_ITEMS}-item limit; keep the plan focused",
                    raw.len()
                )
                .into(),
            ));
        }

        let mut items = Vec::with_capacity(raw.len());
        for (idx, entry) in raw.iter().enumerate() {
            let content = entry
                .get("content")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    Error::ToolExecution(
                        format!("todo[{idx}] is missing a non-empty `content`").into(),
                    )
                })?;
            let status_str = entry
                .get("status")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    Error::ToolExecution(format!("todo[{idx}] is missing `status`").into())
                })?;
            let status = TodoStatus::parse(status_str).ok_or_else(|| {
                Error::ToolExecution(
                    format!(
                        "todo[{idx}] has unknown status {status_str:?}; use pending, in_progress, or completed"
                    )
                    .into(),
                )
            })?;
            items.push(TodoItem::new(content, status));
        }

        // More than one in-progress item defeats the "current focus" signal.
        // Warn rather than reject — the list is still usable, and rejecting
        // would force a model that mislabels into a retry loop.
        let in_progress = items
            .iter()
            .filter(|i| i.status == TodoStatus::InProgress)
            .count();
        let note = (in_progress > 1).then_some(
            "more than one item is in_progress; mark just one to signal your current focus",
        );

        let payload = with_session_context(|ctx| {
            ctx.todos.set(ctx.conversation_id, items.clone());
            list_payload(&items)
        })
        .ok_or_else(|| {
            Error::ToolExecution("todo_write called outside a session context".into())
        })?;

        let mut payload = payload;
        if let Some(note) = note {
            payload["note"] = json!(note);
        }
        Ok(payload)
    }
}

// ── todo_read ───────────────────────────────────────────────────────────

struct TodoReadTool;

#[async_trait]
impl Tool for TodoReadTool {
    fn name(&self) -> &str {
        "todo_read"
    }

    fn description(&self) -> &str {
        "Return your current task list for this conversation (the one you \
         maintain with todo_write). Use it to re-check what's done and what's \
         next — especially after a long stretch of tool calls or once the \
         conversation has been compacted."
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
        let result = with_session_context(|ctx| ctx.todos.get(ctx.conversation_id));
        match result {
            Some(Some(items)) => Ok(list_payload(&items)),
            Some(None) => Ok(json!({
                "empty": true,
                "note": "no task list has been set for this conversation yet; create one with todo_write",
            })),
            None => Err(Error::ToolExecution(
                "todo_read called outside a session context".into(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use rustykrab_core::active_tools::{ActiveToolsRegistry, SessionToolContext};
    use rustykrab_core::capability::CapabilitySet;
    use rustykrab_core::recall::RecallStore;
    use rustykrab_core::{TodoStore, SESSION_TOOL_CONTEXT};
    use uuid::Uuid;

    fn ctx_with_store() -> (SessionToolContext, Uuid, Arc<TodoStore>) {
        let conv = Uuid::new_v4();
        let todos = Arc::new(TodoStore::new());
        let ctx = SessionToolContext {
            conversation_id: conv,
            capabilities: Arc::new(CapabilitySet::none()),
            all_tools: Arc::new(Vec::new()),
            active_tools: Arc::new(ActiveToolsRegistry::new()),
            recall: Arc::new(RecallStore::new()),
            todos: todos.clone(),
        };
        (ctx, conv, todos)
    }

    #[tokio::test]
    async fn write_sets_and_renders_list() {
        let (ctx, conv, store) = ctx_with_store();
        let result = SESSION_TOOL_CONTEXT
            .scope(ctx, async {
                TodoWriteTool
                    .execute(json!({
                        "todos": [
                            {"content": "design", "status": "completed"},
                            {"content": "build", "status": "in_progress"},
                            {"content": "test", "status": "pending"},
                        ]
                    }))
                    .await
            })
            .await
            .unwrap();
        assert_eq!(result["counts"]["total"], 3);
        assert_eq!(result["counts"]["completed"], 1);
        assert_eq!(result["rendered"], "[x] design\n[~] build\n[ ] test");
        // State landed in the store.
        assert_eq!(store.get(conv).unwrap().len(), 3);
    }

    #[tokio::test]
    async fn write_replaces_previous_list() {
        let (ctx, conv, store) = ctx_with_store();
        SESSION_TOOL_CONTEXT
            .scope(ctx, async {
                TodoWriteTool
                    .execute(json!({"todos": [{"content": "old", "status": "pending"}]}))
                    .await
                    .unwrap();
                TodoWriteTool
                    .execute(json!({"todos": [{"content": "new", "status": "in_progress"}]}))
                    .await
                    .unwrap();
            })
            .await;
        let items = store.get(conv).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].content, "new");
    }

    #[tokio::test]
    async fn write_accepts_status_aliases() {
        let (ctx, _conv, store) = ctx_with_store();
        let conv = _conv;
        SESSION_TOOL_CONTEXT
            .scope(ctx, async {
                TodoWriteTool
                    .execute(json!({"todos": [{"content": "x", "status": "done"}]}))
                    .await
                    .unwrap();
            })
            .await;
        assert_eq!(store.get(conv).unwrap()[0].status, TodoStatus::Completed);
    }

    #[tokio::test]
    async fn write_rejects_unknown_status() {
        let (ctx, _, _) = ctx_with_store();
        let err = SESSION_TOOL_CONTEXT
            .scope(ctx, async {
                TodoWriteTool
                    .execute(json!({"todos": [{"content": "x", "status": "frobnicate"}]}))
                    .await
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown status"));
    }

    #[tokio::test]
    async fn write_rejects_empty_content() {
        let (ctx, _, _) = ctx_with_store();
        let err = SESSION_TOOL_CONTEXT
            .scope(ctx, async {
                TodoWriteTool
                    .execute(json!({"todos": [{"content": "   ", "status": "pending"}]}))
                    .await
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("non-empty `content`"));
    }

    #[tokio::test]
    async fn write_warns_on_multiple_in_progress() {
        let (ctx, _, _) = ctx_with_store();
        let result = SESSION_TOOL_CONTEXT
            .scope(ctx, async {
                TodoWriteTool
                    .execute(json!({
                        "todos": [
                            {"content": "a", "status": "in_progress"},
                            {"content": "b", "status": "in_progress"},
                        ]
                    }))
                    .await
            })
            .await
            .unwrap();
        assert!(result["note"].as_str().unwrap().contains("in_progress"));
    }

    #[tokio::test]
    async fn empty_write_clears_list() {
        let (ctx, conv, store) = ctx_with_store();
        SESSION_TOOL_CONTEXT
            .scope(ctx, async {
                TodoWriteTool
                    .execute(json!({"todos": [{"content": "x", "status": "pending"}]}))
                    .await
                    .unwrap();
                TodoWriteTool.execute(json!({"todos": []})).await.unwrap();
            })
            .await;
        assert!(store.get(conv).is_none());
    }

    #[tokio::test]
    async fn read_returns_current_list() {
        let (ctx, _, _) = ctx_with_store();
        let result = SESSION_TOOL_CONTEXT
            .scope(ctx, async {
                TodoWriteTool
                    .execute(json!({"todos": [{"content": "step", "status": "pending"}]}))
                    .await?;
                TodoReadTool.execute(json!({})).await
            })
            .await
            .unwrap();
        assert_eq!(result["counts"]["total"], 1);
        assert_eq!(result["rendered"], "[ ] step");
    }

    #[tokio::test]
    async fn read_reports_empty_when_unset() {
        let (ctx, _, _) = ctx_with_store();
        let result = SESSION_TOOL_CONTEXT
            .scope(ctx, async { TodoReadTool.execute(json!({})).await })
            .await
            .unwrap();
        assert_eq!(result["empty"], true);
    }

    #[tokio::test]
    async fn write_errors_outside_session_scope() {
        let err = TodoWriteTool
            .execute(json!({"todos": [{"content": "x", "status": "pending"}]}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("outside a session context"));
    }

    #[tokio::test]
    async fn write_rejects_too_many_items() {
        let (ctx, _, _) = ctx_with_store();
        let many: Vec<Value> = (0..MAX_TODO_ITEMS + 1)
            .map(|i| json!({"content": format!("item {i}"), "status": "pending"}))
            .collect();
        let err = SESSION_TOOL_CONTEXT
            .scope(ctx, async {
                TodoWriteTool.execute(json!({ "todos": many })).await
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("too many items"));
    }
}
