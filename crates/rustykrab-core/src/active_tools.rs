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
//!
//! # Declared and appended tools
//!
//! A conversation's callable tools come in two kinds (plan
//! `docs/plans/control-layer-and-worker-fleet.md`, section 12):
//!
//! - **declared** (the *active set*): rendered into the tools array of every
//!   request. Chat templates put that array at the front of the prompt, so a
//!   change to it invalidates the whole cached prefix and costs a full
//!   re-prefill (35 s on qwen3.8 at 7K tokens).
//! - **appended**: callable, but delivered to the model as text in a tool
//!   result instead, so the tools array, and with it the cached prefix,
//!   stays as it was. Compaction folds them into the declared set, since it
//!   rewrites the prompt anyway.
//!
//! Which of the two a mid-run request for a tool produces is the
//! conversation's [`LateToolBinding`], set by the runner from its provider's
//! capability data: a provider whose model can call a tool it saw only as
//! text gets `Append`, any other `Rerender`. [`ActiveToolsRegistry::make_callable`]
//! is the one entry point the meta-tools use, so the choice is made in one
//! place and never by a tool looking at a model name.
//!
//! # Searches that found nothing
//!
//! A run also keeps the `tools_list` searches that found nothing
//! ([`SearchMiss`]), so the host can stop a model searching for a tool the
//! catalog does not have: after [`ActiveToolsRegistry::search_miss_limit`]
//! misses of one need, the next is answered as final and recorded as a tool
//! gap ([`ActiveToolsRegistry::tool_gaps`]), which a worker run turns into a
//! typed `capability_gap/tool` for the controller's ladder (control plan
//! section 8, order 2). What counts as the same need is `tools_list`'s rule;
//! the registry only keeps the record, per run.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::capability::CapabilitySet;
use crate::recall::RecallStore;
use crate::todo::TodoStore;
use crate::tool::Tool;

/// How a tool the model asks for mid-run reaches it. See the module docs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LateToolBinding {
    /// Add the tool to the declared tools array: the next request re-renders
    /// the front of the prompt. For providers that refuse a call to a tool
    /// the request did not declare.
    #[default]
    Rerender,
    /// Deliver the tool's schema as text in a tool result and record it as
    /// callable, leaving the declared array untouched. For providers whose
    /// model calls a tool it has only seen as text.
    Append,
}

impl LateToolBinding {
    pub fn as_str(self) -> &'static str {
        match self {
            LateToolBinding::Rerender => "rerender",
            LateToolBinding::Append => "append",
        }
    }
}

/// Misses of one need a run allows before `tools_list` answers the next
/// search for it as final (the harness profile's `tool_search_miss_limit`).
pub const DEFAULT_SEARCH_MISS_LIMIT: usize = 2;

/// A `tools_list` search that found nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchMiss {
    /// The need, as `tools_list` normalises it (stemmed terms, sorted).
    pub terms: Vec<String>,
    /// The query as the model wrote it.
    pub query: String,
    /// The tools the answer named as non-matches.
    pub near: Vec<String>,
}

/// What [`ActiveToolsRegistry::make_callable`] did with each name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Callable {
    /// Names that became callable by this call, in the order asked.
    pub newly: Vec<String>,
    /// Names that were callable already (declared or appended).
    pub already: Vec<String>,
    /// How the newly callable names were delivered.
    pub binding: LateToolBinding,
}

/// Per-conversation active set plus a change counter, so consumers can
/// cache work derived from the set (e.g. the runner's schema list) and
/// invalidate only when the set actually changes.
#[derive(Debug, Default)]
struct ActiveEntry {
    /// The declared set: rendered into every request's tools array.
    names: HashSet<String>,
    /// Callable tools delivered as text. Disjoint from `names`.
    appended: HashSet<String>,
    /// How a mid-run request for a tool is delivered.
    binding: LateToolBinding,
    /// Bumped only when `names` changes: the version tracks the tools
    /// array, which is what caches (and the prompt prefix) depend on.
    version: u64,
    /// This run's searches that found nothing, oldest first.
    misses: Vec<SearchMiss>,
    /// Misses of one need before the next is final; `None` takes
    /// [`DEFAULT_SEARCH_MISS_LIMIT`].
    miss_limit: Option<usize>,
    /// Needs this run's searching gave up on, as first worded.
    gaps: Vec<String>,
}

impl ActiveEntry {
    /// A fresh entry pre-populated with the registry's seed. Version stays
    /// 0: the seed is where every conversation starts, not a change to it.
    fn seeded(seed: &HashSet<String>) -> Self {
        Self {
            names: seed.clone(),
            ..Self::default()
        }
    }
}

/// Tracks which tools are "active" for each conversation.
///
/// Conversations start with the seed set (empty unless one was given). The
/// host activates a run's visible set before its first model call; after
/// that, `tools_list` and `tools_load` go through
/// [`make_callable`](Self::make_callable), which appends or activates by the
/// conversation's [`LateToolBinding`]. The runner sends the schemas of
/// (meta tools) ∪ (active set); appended tools are callable without being
/// sent.
#[derive(Debug, Default)]
pub struct ActiveToolsRegistry {
    inner: RwLock<HashMap<Uuid, ActiveEntry>>,
    /// Names every conversation starts with, in addition to whatever
    /// `tools_load` turns on later. Empty for a normal deployment, where
    /// discovery is the point; used when the registry is small and known
    /// up front, so requiring a `tools_load` round-trip to reach it would
    /// be pure overhead.
    seed: HashSet<String>,
}

impl ActiveToolsRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry whose conversations all start with `names` already
    /// active.
    pub fn with_seed<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            inner: RwLock::new(HashMap::new()),
            seed: names.into_iter().map(Into::into).collect(),
        }
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
        let entry = guard
            .entry(conversation_id)
            .or_insert_with(|| ActiveEntry::seeded(&self.seed));
        let mut changed = false;
        for name in names {
            let name = name.into();
            entry.appended.remove(&name);
            changed |= entry.names.insert(name);
        }
        if changed {
            entry.version += 1;
        }
    }

    /// Record tools as callable for a conversation without declaring them:
    /// the caller delivers their schemas as text. Leaves the
    /// [`version`](Self::version), and so the tools array, unchanged.
    /// Returns the names that were not callable before, in the order given.
    pub fn append<I, S>(&self, conversation_id: Uuid, names: I) -> Vec<String>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let entry = guard
            .entry(conversation_id)
            .or_insert_with(|| ActiveEntry::seeded(&self.seed));
        let mut newly = Vec::new();
        for name in names {
            let name = name.into();
            if !entry.names.contains(&name) && entry.appended.insert(name.clone()) {
                newly.push(name);
            }
        }
        newly
    }

    /// Make tools callable the way this conversation's [`LateToolBinding`]
    /// says: appended under `Append`, activated under `Rerender`. Names
    /// already callable are reported and left alone, so asking again is
    /// free under either binding.
    pub fn make_callable<I, S>(&self, conversation_id: Uuid, names: I) -> Callable
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let entry = guard
            .entry(conversation_id)
            .or_insert_with(|| ActiveEntry::seeded(&self.seed));
        let mut out = Callable {
            binding: entry.binding,
            ..Callable::default()
        };
        for name in names {
            let name = name.into();
            if entry.names.contains(&name) || entry.appended.contains(&name) {
                if !out.already.contains(&name) {
                    out.already.push(name);
                }
                continue;
            }
            match entry.binding {
                LateToolBinding::Append => {
                    entry.appended.insert(name.clone());
                }
                LateToolBinding::Rerender => {
                    entry.names.insert(name.clone());
                }
            }
            out.newly.push(name);
        }
        if entry.binding == LateToolBinding::Rerender && !out.newly.is_empty() {
            entry.version += 1;
        }
        out
    }

    /// Move every appended tool into the declared set. For compaction,
    /// which rewrites the prompt and re-prefills anyway, so the appended
    /// schemas it displaces from the history live on in the tools array.
    /// Returns the folded names, sorted; bumps the version when any.
    pub fn fold_appended(&self, conversation_id: Uuid) -> Vec<String> {
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let Some(entry) = guard.get_mut(&conversation_id) else {
            return Vec::new();
        };
        let mut folded: Vec<String> = entry.appended.drain().collect();
        folded.sort();
        let mut changed = false;
        for name in &folded {
            changed |= entry.names.insert(name.clone());
        }
        if changed {
            entry.version += 1;
        }
        folded
    }

    /// Set how mid-run tool requests are delivered in a conversation. The
    /// runner calls this at the start of every run, from its provider's
    /// capability data or the harness profile's override.
    pub fn set_late_binding(&self, conversation_id: Uuid, binding: LateToolBinding) {
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        guard
            .entry(conversation_id)
            .or_insert_with(|| ActiveEntry::seeded(&self.seed))
            .binding = binding;
    }

    /// Start a run's record of tool searches afresh: no misses, no gaps,
    /// and `limit` misses of one need before the next search for it is
    /// answered as final. The runner calls this at the start of every run.
    pub fn begin_tool_search(&self, conversation_id: Uuid, limit: usize) {
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let entry = guard
            .entry(conversation_id)
            .or_insert_with(|| ActiveEntry::seeded(&self.seed));
        entry.misses.clear();
        entry.gaps.clear();
        entry.miss_limit = Some(limit);
    }

    /// Misses of one need this conversation's run allows before the next
    /// search for it is answered as final.
    pub fn search_miss_limit(&self, conversation_id: Uuid) -> usize {
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        guard
            .get(&conversation_id)
            .and_then(|entry| entry.miss_limit)
            .unwrap_or(DEFAULT_SEARCH_MISS_LIMIT)
    }

    /// Record a search that found nothing.
    pub fn record_search_miss(&self, conversation_id: Uuid, miss: SearchMiss) {
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        guard
            .entry(conversation_id)
            .or_insert_with(|| ActiveEntry::seeded(&self.seed))
            .misses
            .push(miss);
    }

    /// This run's searches that found nothing, oldest first.
    pub fn search_misses(&self, conversation_id: Uuid) -> Vec<SearchMiss> {
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        guard
            .get(&conversation_id)
            .map(|entry| entry.misses.clone())
            .unwrap_or_default()
    }

    /// Record that searching gave up on `need`: no tool provides it. Once
    /// per need.
    pub fn record_tool_gap(&self, conversation_id: Uuid, need: impl Into<String>) {
        let need = need.into();
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let gaps = &mut guard
            .entry(conversation_id)
            .or_insert_with(|| ActiveEntry::seeded(&self.seed))
            .gaps;
        if !gaps.contains(&need) {
            gaps.push(need);
        }
    }

    /// The needs this run's searching gave up on, oldest first.
    pub fn tool_gaps(&self, conversation_id: Uuid) -> Vec<String> {
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        guard
            .get(&conversation_id)
            .map(|entry| entry.gaps.clone())
            .unwrap_or_default()
    }

    /// How mid-run tool requests are delivered in a conversation;
    /// `Rerender` until a runner says otherwise.
    pub fn late_binding(&self, conversation_id: Uuid) -> LateToolBinding {
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        guard
            .get(&conversation_id)
            .map(|entry| entry.binding)
            .unwrap_or_default()
    }

    /// The tools appended to a conversation (callable, not declared), sorted.
    pub fn appended_for(&self, conversation_id: Uuid) -> Vec<String> {
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let mut names: Vec<String> = guard
            .get(&conversation_id)
            .map(|entry| entry.appended.iter().cloned().collect())
            .unwrap_or_default();
        names.sort();
        names
    }

    /// Whether a tool was appended to a conversation.
    pub fn is_appended(&self, conversation_id: Uuid, name: &str) -> bool {
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        guard
            .get(&conversation_id)
            .is_some_and(|entry| entry.appended.contains(name))
    }

    /// Whether the model may call a tool in a conversation: declared or
    /// appended. Registration and capabilities are the caller's to check.
    pub fn is_callable(&self, conversation_id: Uuid, name: &str) -> bool {
        self.is_active(conversation_id, name) || self.is_appended(conversation_id, name)
    }

    /// Every declared or appended tool name of a conversation.
    pub fn callable_for(&self, conversation_id: Uuid) -> HashSet<String> {
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        match guard.get(&conversation_id) {
            Some(entry) => entry.names.union(&entry.appended).cloned().collect(),
            None => self.seed.clone(),
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
            .unwrap_or_else(|| self.seed.clone())
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
            None => f(0, &self.seed),
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
            .unwrap_or_else(|| self.seed.contains(name))
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

    #[test]
    fn append_is_callable_without_touching_the_tools_array() {
        let reg = ActiveToolsRegistry::new();
        let conv = Uuid::new_v4();
        reg.activate(conv, ["read"]);
        let v = reg.version(conv);

        assert_eq!(reg.append(conv, ["get_weather", "read"]), ["get_weather"]);
        assert_eq!(
            reg.version(conv),
            v,
            "an append is not a tools-array change"
        );
        assert!(reg.is_callable(conv, "get_weather"));
        assert!(reg.is_appended(conv, "get_weather"));
        assert!(!reg.is_active(conv, "get_weather"));
        assert!(reg.is_callable(conv, "read") && !reg.is_appended(conv, "read"));
        assert_eq!(reg.append(conv, ["get_weather"]), Vec::<String>::new());
        let callable = reg.callable_for(conv);
        assert!(callable.contains("read") && callable.contains("get_weather"));
    }

    #[test]
    fn make_callable_follows_the_conversations_binding() {
        let reg = ActiveToolsRegistry::new();
        let rerender = Uuid::new_v4();
        let append = Uuid::new_v4();
        reg.set_late_binding(append, LateToolBinding::Append);
        assert_eq!(reg.late_binding(rerender), LateToolBinding::Rerender);
        assert_eq!(reg.late_binding(append), LateToolBinding::Append);

        let r = reg.make_callable(rerender, ["browser"]);
        assert_eq!(r.newly, ["browser"]);
        assert_eq!(r.binding, LateToolBinding::Rerender);
        assert!(reg.is_active(rerender, "browser"));
        assert!(
            reg.version(rerender) > 0,
            "rerender changes the tools array"
        );

        let a = reg.make_callable(append, ["browser"]);
        assert_eq!(a.newly, ["browser"]);
        assert_eq!(a.binding, LateToolBinding::Append);
        assert!(reg.is_appended(append, "browser"));
        assert_eq!(
            reg.version(append),
            0,
            "append leaves the tools array alone"
        );

        // Asking again is a no-op that says so, under either binding.
        for conv in [rerender, append] {
            let again = reg.make_callable(conv, ["browser", "browser"]);
            assert!(again.newly.is_empty());
            assert_eq!(again.already, ["browser"]);
        }
    }

    #[test]
    fn compaction_folds_appended_tools_into_the_declared_set() {
        let reg = ActiveToolsRegistry::new();
        let conv = Uuid::new_v4();
        reg.set_late_binding(conv, LateToolBinding::Append);
        reg.make_callable(conv, ["track_package", "get_weather"]);
        assert_eq!(reg.version(conv), 0);

        assert_eq!(reg.fold_appended(conv), ["get_weather", "track_package"]);
        assert!(reg.version(conv) > 0);
        assert!(reg.is_active(conv, "track_package"));
        assert!(reg.appended_for(conv).is_empty());
        assert!(reg.fold_appended(conv).is_empty());
        assert!(reg.fold_appended(Uuid::new_v4()).is_empty());
    }

    #[test]
    fn activating_an_appended_tool_declares_it_once() {
        let reg = ActiveToolsRegistry::new();
        let conv = Uuid::new_v4();
        reg.append(conv, ["exec"]);
        reg.activate(conv, ["exec"]);
        assert!(reg.is_active(conv, "exec"));
        assert!(!reg.is_appended(conv, "exec"));
        assert!(reg.appended_for(conv).is_empty());
    }

    #[test]
    fn a_run_keeps_its_search_misses_and_gaps_and_the_next_run_starts_clean() {
        let reg = ActiveToolsRegistry::new();
        let conv = Uuid::new_v4();
        assert_eq!(reg.search_miss_limit(conv), DEFAULT_SEARCH_MISS_LIMIT);
        reg.begin_tool_search(conv, 3);
        assert_eq!(reg.search_miss_limit(conv), 3);
        let miss = SearchMiss {
            terms: vec!["current".into(), "weather".into()],
            query: "current weather".into(),
            near: vec!["get_forecast".into()],
        };
        reg.record_search_miss(conv, miss.clone());
        reg.record_tool_gap(conv, "current weather");
        reg.record_tool_gap(conv, "current weather");
        assert_eq!(reg.search_misses(conv), [miss]);
        assert_eq!(reg.tool_gaps(conv), ["current weather"]);
        assert!(reg.search_misses(Uuid::new_v4()).is_empty());

        reg.begin_tool_search(conv, 2);
        assert!(reg.search_misses(conv).is_empty());
        assert!(reg.tool_gaps(conv).is_empty());
        assert_eq!(reg.search_miss_limit(conv), 2);
    }
}
