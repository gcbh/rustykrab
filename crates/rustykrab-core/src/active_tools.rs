//! Per-session "active tools" registry used by the `tools_list` / `tools_load`
//! meta-tools.
//!
//! The meta-tools let an agent discover the full tool catalog and then
//! selectively load subsets of tools into its active context, keeping the
//! per-request schema payload small. This module provides:
//!
//! - [`ActiveToolsRegistry`] — a thread-safe map from conversation id to the
//!   set of tool names currently active for that conversation.
//! - [`SESSION_TOOL_CONTEXT`] — a [`tokio::task_local`] that threads the
//!   currently-executing session's conversation id, capability set, and tool
//!   catalog to any tool invoked inside the agent runner's scope.
//!
//! The runner wraps its loop in [`SESSION_TOOL_CONTEXT::scope`]; meta-tools
//! read the context via [`with_session_context`] to know which conversation
//! they belong to.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use uuid::Uuid;

use crate::capability::CapabilitySet;
use crate::recall::RecallStore;
use crate::tool::Tool;

/// Tracks which tools are "active" for each conversation.
///
/// Conversations start with an empty active set. The meta-tool `tools_load`
/// populates it; the runner filters the schemas sent to the model down to
/// (meta tools) ∪ (active set).
#[derive(Debug, Default)]
pub struct ActiveToolsRegistry {
    inner: RwLock<HashMap<Uuid, HashSet<String>>>,
}

impl ActiveToolsRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark the given tools as active for a conversation.
    pub fn activate<I, S>(&self, conversation_id: Uuid, names: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let entry = guard.entry(conversation_id).or_default();
        for name in names {
            entry.insert(name.into());
        }
    }

    /// Return a snapshot of the active tool names for a conversation.
    pub fn active_for(&self, conversation_id: Uuid) -> HashSet<String> {
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        guard.get(&conversation_id).cloned().unwrap_or_default()
    }

    /// Check whether a specific tool is active for a conversation.
    pub fn is_active(&self, conversation_id: Uuid, name: &str) -> bool {
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        guard
            .get(&conversation_id)
            .map(|set| set.contains(name))
            .unwrap_or(false)
    }

    /// Forget the active set for a conversation (used on session teardown).
    pub fn clear(&self, conversation_id: Uuid) {
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        guard.remove(&conversation_id);
    }
}

/// Context made available to tools invoked inside the runner's scope.
#[derive(Clone)]
pub struct SessionToolContext {
    pub conversation_id: Uuid,
    pub capabilities: Arc<CapabilitySet>,
    pub all_tools: Arc<Vec<Arc<dyn Tool>>>,
    pub active_tools: Arc<ActiveToolsRegistry>,
    /// Per-conversation archive of compaction-displaced history.  The
    /// `recall_*` tools read from this so the model can recover detail
    /// the compaction summary dropped.
    pub recall: Arc<RecallStore>,
}

tokio::task_local! {
    pub static SESSION_TOOL_CONTEXT: SessionToolContext;
}

/// Run `f` with the current session's tool context, if one has been set by
/// the enclosing runner. Returns `None` if invoked outside a runner scope.
pub fn with_session_context<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&SessionToolContext) -> R,
{
    SESSION_TOOL_CONTEXT.try_with(|ctx| f(ctx)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activate_and_query() {
        let reg = ActiveToolsRegistry::new();
        let conv = Uuid::new_v4();
        assert!(reg.active_for(conv).is_empty());
        reg.activate(conv, ["read", "write"]);
        assert!(reg.is_active(conv, "read"));
        assert!(reg.is_active(conv, "write"));
        assert!(!reg.is_active(conv, "exec"));
        let active = reg.active_for(conv);
        assert_eq!(active.len(), 2);
    }

    #[test]
    fn conversations_are_isolated() {
        let reg = ActiveToolsRegistry::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        reg.activate(a, ["read"]);
        reg.activate(b, ["write"]);
        assert!(reg.is_active(a, "read"));
        assert!(!reg.is_active(a, "write"));
        assert!(reg.is_active(b, "write"));
        assert!(!reg.is_active(b, "read"));
    }

    #[test]
    fn clear_removes_session_entry() {
        let reg = ActiveToolsRegistry::new();
        let conv = Uuid::new_v4();
        reg.activate(conv, ["read"]);
        reg.clear(conv);
        assert!(reg.active_for(conv).is_empty());
    }
}
