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
use crate::todo::TodoStore;
use crate::tool::Tool;

/// Per-conversation active set plus a change counter, so consumers can
/// cache work derived from the set (e.g. the runner's schema list) and
/// invalidate only when the set actually changes.
#[derive(Debug, Default)]
struct ActiveEntry {
    names: HashSet<String>,
    version: u64,
}

/// Tracks which tools are "active" for each conversation.
///
/// Conversations start with an empty active set. The meta-tool `tools_load`
/// populates it; the runner filters the schemas sent to the model down to
/// (meta tools) ∪ (active set).
#[derive(Debug, Default)]
pub struct ActiveToolsRegistry {
    inner: RwLock<HashMap<Uuid, ActiveEntry>>,
}

impl ActiveToolsRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark the given tools as active for a conversation. Bumps the
    /// conversation's [`version`](Self::version) only when at least one
    /// name is newly inserted, so idempotent re-activation stays free for
    /// version-keyed caches.
    pub fn activate<I, S>(&self, conversation_id: Uuid, names: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let entry = guard.entry(conversation_id).or_default();
        let mut changed = false;
        for name in names {
            changed |= entry.names.insert(name.into());
        }
        if changed {
            entry.version += 1;
        }
    }

    /// Return a snapshot of the active tool names for a conversation.
    ///
    /// Clones the set; prefer [`with_active`](Self::with_active) on hot
    /// paths that only need to inspect it.
    pub fn active_for(&self, conversation_id: Uuid) -> HashSet<String> {
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        guard
            .get(&conversation_id)
            .map(|entry| entry.names.clone())
            .unwrap_or_default()
    }

    /// Run `f` against the active set for a conversation without cloning
    /// it. `f` also receives the set's current version (0 when nothing has
    /// ever been activated), read under the same lock so the pair is a
    /// consistent snapshot for version-keyed caches.
    pub fn with_active<R>(
        &self,
        conversation_id: Uuid,
        f: impl FnOnce(u64, &HashSet<String>) -> R,
    ) -> R {
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        match guard.get(&conversation_id) {
            Some(entry) => f(entry.version, &entry.names),
            None => f(0, &HashSet::new()),
        }
    }

    /// Current version of a conversation's active set. Starts at 0 (no
    /// activations yet) and increments every time [`activate`](Self::activate)
    /// actually changes the set. Consumers can compare versions to decide
    /// whether cached derivations of the set are still valid.
    pub fn version(&self, conversation_id: Uuid) -> u64 {
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        guard
            .get(&conversation_id)
            .map(|entry| entry.version)
            .unwrap_or(0)
    }

    /// Check whether a specific tool is active for a conversation.
    pub fn is_active(&self, conversation_id: Uuid, name: &str) -> bool {
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        guard
            .get(&conversation_id)
            .map(|entry| entry.names.contains(name))
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
    /// Per-conversation todo list.  The `todo_write` / `todo_read` tools
    /// maintain it, and the runner re-emits it verbatim across compaction
    /// so the agent's plan survives the churn.
    pub todos: Arc<TodoStore>,
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

    #[test]
    fn version_bumps_only_on_real_changes() {
        let reg = ActiveToolsRegistry::new();
        let conv = Uuid::new_v4();
        assert_eq!(reg.version(conv), 0);

        reg.activate(conv, ["read", "write"]);
        let v1 = reg.version(conv);
        assert!(v1 > 0);

        // Idempotent re-activation must not invalidate version-keyed caches.
        reg.activate(conv, ["read", "write"]);
        assert_eq!(reg.version(conv), v1);

        // A genuinely new name bumps the version.
        reg.activate(conv, ["exec"]);
        assert!(reg.version(conv) > v1);
    }

    #[test]
    fn with_active_exposes_consistent_snapshot() {
        let reg = ActiveToolsRegistry::new();
        let conv = Uuid::new_v4();

        // Missing entry: version 0, empty set.
        reg.with_active(conv, |version, names| {
            assert_eq!(version, 0);
            assert!(names.is_empty());
        });

        reg.activate(conv, ["read"]);
        let version = reg.with_active(conv, |version, names| {
            assert!(names.contains("read"));
            version
        });
        assert_eq!(version, reg.version(conv));
    }
}
