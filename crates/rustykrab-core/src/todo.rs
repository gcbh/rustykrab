//! Per-conversation todo list — an intra-loop planning scratchpad.
//!
//! Long-horizon agent runs drift: as the prompt fills with tool output and
//! intermediate reasoning, the original plan gets buried, and compaction
//! (see `rustykrab-agent`'s `compact_history`) can summarise it away. A
//! structured todo list is the standard remedy — a cheap, high-signal
//! artifact the agent writes once and rewrites as it works, so the goal
//! stays in view even after the surrounding context has churned. It is a
//! scratchpad for *intentions* rather than a record of facts (which is what
//! [`crate::recall::RecallStore`] holds).
//!
//! [`TodoStore`] holds the current list per conversation. The `todo_write`
//! / `todo_read` tools in `rustykrab-agent` mutate and inspect it, and the
//! runner re-emits the rendered list verbatim across a compaction so the
//! checklist survives the one event most likely to lose it.
//!
//! Unlike [`crate::recall::RecallStore`], this is in-memory only: a todo
//! list is short-horizon working state for the active run, re-derivable by
//! the model at any time, not durable history worth a SQLite round-trip.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Lifecycle state of a single todo item. Mirrors the de-facto standard
/// three-state model (pending → in_progress → completed) used by agent
/// todo-list tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

impl TodoStatus {
    /// Parse a status from its wire string, tolerating the common aliases
    /// models emit (`done`, `doing`, `todo`, hyphenated `in-progress`).
    /// Returns `None` for anything unrecognised so the tool layer can
    /// surface a precise error rather than silently defaulting.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "pending" | "todo" | "open" | "not_started" | "not-started" => Some(Self::Pending),
            "in_progress" | "in-progress" | "inprogress" | "active" | "doing" => {
                Some(Self::InProgress)
            }
            "completed" | "complete" | "done" | "finished" => Some(Self::Completed),
            _ => None,
        }
    }

    /// Markdown checkbox marker used when rendering the list. `[~]` marks the
    /// single in-progress item so the current focus is visible at a glance.
    pub fn marker(self) -> &'static str {
        match self {
            Self::Pending => "[ ]",
            Self::InProgress => "[~]",
            Self::Completed => "[x]",
        }
    }

    /// Canonical wire string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }
}

/// A single checklist entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    /// What the step is — imperative and short ("Add the migration").
    pub content: String,
    /// Where the step is in its lifecycle.
    pub status: TodoStatus,
}

impl TodoItem {
    pub fn new(content: impl Into<String>, status: TodoStatus) -> Self {
        Self {
            content: content.into(),
            status,
        }
    }
}

/// Render a list of items as a markdown checklist, or `None` when the list
/// is empty so callers can skip emitting an empty block.
pub fn render_todos(items: &[TodoItem]) -> Option<String> {
    if items.is_empty() {
        return None;
    }
    let mut out = String::new();
    for item in items {
        out.push_str(item.status.marker());
        out.push(' ');
        out.push_str(item.content.trim());
        out.push('\n');
    }
    Some(out.trim_end().to_string())
}

/// Thread-safe map from conversation id to its current todo list.
///
/// Replace semantics: [`set`](Self::set) overwrites the whole list for a
/// conversation (the canonical "rewrite the full checklist on each update"
/// model), so the store never holds a stale partial list, and an empty
/// write clears it.
#[derive(Debug, Default)]
pub struct TodoStore {
    inner: RwLock<HashMap<Uuid, Arc<Vec<TodoItem>>>>,
}

impl TodoStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the todo list for a conversation. An empty `items` clears it.
    pub fn set(&self, conversation_id: Uuid, items: Vec<TodoItem>) {
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        if items.is_empty() {
            guard.remove(&conversation_id);
        } else {
            guard.insert(conversation_id, Arc::new(items));
        }
    }

    /// Return a cheap clone of the current list, or `None` if unset/empty.
    pub fn get(&self, conversation_id: Uuid) -> Option<Arc<Vec<TodoItem>>> {
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        guard.get(&conversation_id).cloned()
    }

    /// Render the current list as a markdown checklist, or `None` if unset.
    pub fn render(&self, conversation_id: Uuid) -> Option<String> {
        self.get(conversation_id)
            .and_then(|items| render_todos(&items))
    }

    /// Forget the list for a conversation (session teardown / deletion).
    pub fn clear(&self, conversation_id: Uuid) {
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        guard.remove(&conversation_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_aliases() {
        assert_eq!(TodoStatus::parse("pending"), Some(TodoStatus::Pending));
        assert_eq!(TodoStatus::parse(" TODO "), Some(TodoStatus::Pending));
        assert_eq!(
            TodoStatus::parse("in-progress"),
            Some(TodoStatus::InProgress)
        );
        assert_eq!(TodoStatus::parse("doing"), Some(TodoStatus::InProgress));
        assert_eq!(TodoStatus::parse("done"), Some(TodoStatus::Completed));
        assert_eq!(TodoStatus::parse("nonsense"), None);
    }

    #[test]
    fn render_produces_markdown_checklist() {
        let items = vec![
            TodoItem::new("design the store", TodoStatus::Completed),
            TodoItem::new("wire the tool", TodoStatus::InProgress),
            TodoItem::new("add tests", TodoStatus::Pending),
        ];
        let rendered = render_todos(&items).unwrap();
        assert_eq!(
            rendered,
            "[x] design the store\n[~] wire the tool\n[ ] add tests"
        );
    }

    #[test]
    fn render_empty_is_none() {
        assert!(render_todos(&[]).is_none());
    }

    #[test]
    fn set_and_get_roundtrips() {
        let store = TodoStore::new();
        let conv = Uuid::new_v4();
        assert!(store.get(conv).is_none());
        store.set(conv, vec![TodoItem::new("step one", TodoStatus::Pending)]);
        let got = store.get(conv).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].content, "step one");
    }

    #[test]
    fn set_replaces_rather_than_appends() {
        let store = TodoStore::new();
        let conv = Uuid::new_v4();
        store.set(conv, vec![TodoItem::new("old", TodoStatus::Pending)]);
        store.set(
            conv,
            vec![
                TodoItem::new("new a", TodoStatus::InProgress),
                TodoItem::new("new b", TodoStatus::Pending),
            ],
        );
        let got = store.get(conv).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].content, "new a");
    }

    #[test]
    fn empty_set_clears() {
        let store = TodoStore::new();
        let conv = Uuid::new_v4();
        store.set(conv, vec![TodoItem::new("x", TodoStatus::Pending)]);
        store.set(conv, vec![]);
        assert!(store.get(conv).is_none());
    }

    #[test]
    fn render_via_store() {
        let store = TodoStore::new();
        let conv = Uuid::new_v4();
        store.set(
            conv,
            vec![TodoItem::new("only step", TodoStatus::InProgress)],
        );
        assert_eq!(store.render(conv).as_deref(), Some("[~] only step"));
    }

    #[test]
    fn conversations_are_isolated() {
        let store = TodoStore::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        store.set(a, vec![TodoItem::new("a-step", TodoStatus::Pending)]);
        store.set(b, vec![TodoItem::new("b-step", TodoStatus::Pending)]);
        assert_eq!(store.get(a).unwrap()[0].content, "a-step");
        assert_eq!(store.get(b).unwrap()[0].content, "b-step");
    }

    #[test]
    fn clear_drops_entry() {
        let store = TodoStore::new();
        let conv = Uuid::new_v4();
        store.set(conv, vec![TodoItem::new("x", TodoStatus::Pending)]);
        store.clear(conv);
        assert!(store.get(conv).is_none());
    }
}
