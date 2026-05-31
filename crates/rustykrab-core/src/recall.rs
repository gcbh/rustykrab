//! Per-conversation recall store for compaction-displaced history.
//!
//! Compaction in `rustykrab-agent` summarises long conversations into a
//! short bullet list, then drops the original messages from the prompt.
//! That keeps prompts cheap, but the dropped detail (concrete numbers,
//! file paths, intermediate tool outputs) is gone from the model's view.
//!
//! [`RecallStore`] preserves that detail out-of-band: the runner appends
//! the rendered text of every dropped batch into a per-conversation
//! buffer here, and a small set of REPL-style `recall_*` tools lets the
//! agent inspect it on demand. This mirrors the foundational RLM paper
//! (Zhang, Kraska, Khattab — arXiv 2512.24601): the long context lives
//! outside the prompt, and the model navigates it via tools rather than
//! re-reading it wholesale.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use uuid::Uuid;

/// Durable backing layer for the recall archive, keyed by conversation id.
///
/// Implemented in `rustykrab-store` (SQLite) and injected into
/// [`RecallStore`] so this crate stays storage-agnostic. All methods are
/// **best-effort**: implementations must log and swallow their own errors
/// so a persistence hiccup never breaks the agent loop (which is why
/// `upsert`/`delete` return `()` and `load` returns `Option` rather than
/// `Result`).
pub trait RecallPersistence: Send + Sync + std::fmt::Debug {
    /// Load the full archive text for a conversation, or `None` if there
    /// is no persisted archive (or the load failed).
    fn load(&self, conversation_id: Uuid) -> Option<String>;
    /// Insert or replace the persisted archive text for a conversation.
    fn upsert(&self, conversation_id: Uuid, archive: &str);
    /// Permanently delete the persisted archive for a conversation.
    fn delete(&self, conversation_id: Uuid);
}

/// Thread-safe map from conversation id to the rendered text of all
/// messages that have been displaced by compaction.
///
/// Append-only within a conversation: each compaction appends the
/// rendered batch, joined to the previous archive with a blank line.
///
/// When constructed via [`RecallStore::with_persistence`], the in-memory
/// map acts as a write-through cache over a durable backing store: writes
/// are mirrored to the backend, and on a cache miss the entry is lazily
/// hydrated from it. This lets the archive survive process restarts — a
/// resumed conversation re-hydrates its history on first `recall_*` access.
#[derive(Debug, Default)]
pub struct RecallStore {
    inner: RwLock<HashMap<Uuid, Arc<String>>>,
    persistence: Option<Arc<dyn RecallPersistence>>,
}

impl RecallStore {
    /// Construct a purely in-memory store with no durable backing. The
    /// archive is lost when the process exits.
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct a store backed by a durable persistence layer. The
    /// in-memory map becomes a write-through cache that is lazily hydrated
    /// from `persistence` on first access per conversation.
    pub fn with_persistence(persistence: Arc<dyn RecallPersistence>) -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
            persistence: Some(persistence),
        }
    }

    /// Ensure the in-memory cache holds the persisted archive for
    /// `conversation_id` (if any). No-op when there is no persistence
    /// layer or the entry is already cached. The durable load happens
    /// outside the cache lock so SQLite I/O never serialises behind it.
    fn hydrate(&self, conversation_id: Uuid) {
        let Some(persistence) = &self.persistence else {
            return;
        };
        {
            let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
            if guard.contains_key(&conversation_id) {
                return;
            }
        }
        if let Some(text) = persistence.load(conversation_id) {
            let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
            // `or_insert_with` guards against a concurrent writer that
            // populated the entry while we were loading.
            guard
                .entry(conversation_id)
                .or_insert_with(|| Arc::new(text));
        }
    }

    /// Append `text` to the archive for `conversation_id`. If an entry
    /// already exists, the new text is concatenated with a blank-line
    /// separator so successive batches stay readable.
    pub fn append(&self, conversation_id: Uuid, text: &str) {
        if text.is_empty() {
            return;
        }
        // Hydrate first so we concatenate onto any prior (possibly
        // persisted-then-restarted) archive rather than silently restarting.
        self.hydrate(conversation_id);
        let combined = {
            let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
            let entry = guard.entry(conversation_id).or_default();
            let combined = if entry.is_empty() {
                Arc::new(text.to_string())
            } else {
                Arc::new(format!("{}\n\n{}", entry.as_str(), text))
            };
            *entry = Arc::clone(&combined);
            combined
        };
        // Mirror to durable storage after releasing the cache lock.
        if let Some(persistence) = &self.persistence {
            persistence.upsert(conversation_id, &combined);
        }
    }

    /// Return a cheap clone of the archive for `conversation_id`, or
    /// `None` if nothing has been archived yet.
    pub fn get(&self, conversation_id: Uuid) -> Option<Arc<String>> {
        self.hydrate(conversation_id);
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        guard.get(&conversation_id).cloned()
    }

    /// Drop the in-memory cache entry for a conversation. The durable
    /// archive (if any) is left intact so it can be lazily re-hydrated on
    /// next access; use [`purge`](Self::purge) to delete it permanently.
    /// Intended for session teardown, where we want to free memory without
    /// forgetting history.
    pub fn clear(&self, conversation_id: Uuid) {
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        guard.remove(&conversation_id);
    }

    /// Permanently delete the archive for a conversation from both the
    /// in-memory cache and the durable store. Use when a conversation is
    /// deleted outright.
    pub fn purge(&self, conversation_id: Uuid) {
        {
            let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
            guard.remove(&conversation_id);
        }
        if let Some(persistence) = &self.persistence {
            persistence.delete(conversation_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// In-memory stand-in for a SQLite-backed [`RecallPersistence`].
    #[derive(Debug, Default)]
    struct MockPersistence {
        rows: Mutex<HashMap<Uuid, String>>,
        loads: Mutex<u32>,
    }

    impl RecallPersistence for MockPersistence {
        fn load(&self, conversation_id: Uuid) -> Option<String> {
            *self.loads.lock().unwrap() += 1;
            self.rows.lock().unwrap().get(&conversation_id).cloned()
        }
        fn upsert(&self, conversation_id: Uuid, archive: &str) {
            self.rows
                .lock()
                .unwrap()
                .insert(conversation_id, archive.to_string());
        }
        fn delete(&self, conversation_id: Uuid) {
            self.rows.lock().unwrap().remove(&conversation_id);
        }
    }

    #[test]
    fn append_creates_entry() {
        let store = RecallStore::new();
        let conv = Uuid::new_v4();
        store.append(conv, "first batch");
        let got = store.get(conv).expect("archive should exist");
        assert_eq!(got.as_str(), "first batch");
    }

    #[test]
    fn append_concatenates_with_blank_line() {
        let store = RecallStore::new();
        let conv = Uuid::new_v4();
        store.append(conv, "first");
        store.append(conv, "second");
        let got = store.get(conv).unwrap();
        assert_eq!(got.as_str(), "first\n\nsecond");
    }

    #[test]
    fn empty_text_is_a_noop() {
        let store = RecallStore::new();
        let conv = Uuid::new_v4();
        store.append(conv, "");
        assert!(store.get(conv).is_none());
    }

    #[test]
    fn conversations_are_isolated() {
        let store = RecallStore::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        store.append(a, "a-text");
        store.append(b, "b-text");
        assert_eq!(store.get(a).unwrap().as_str(), "a-text");
        assert_eq!(store.get(b).unwrap().as_str(), "b-text");
    }

    #[test]
    fn clear_drops_entry() {
        let store = RecallStore::new();
        let conv = Uuid::new_v4();
        store.append(conv, "x");
        store.clear(conv);
        assert!(store.get(conv).is_none());
    }

    #[test]
    fn append_mirrors_to_persistence() {
        let backend = Arc::new(MockPersistence::default());
        let store = RecallStore::with_persistence(backend.clone());
        let conv = Uuid::new_v4();
        store.append(conv, "first");
        store.append(conv, "second");
        assert_eq!(
            backend.rows.lock().unwrap().get(&conv).unwrap(),
            "first\n\nsecond"
        );
    }

    #[test]
    fn hydrates_from_persistence_after_restart() {
        let backend = Arc::new(MockPersistence::default());
        let conv = Uuid::new_v4();
        backend.upsert(conv, "archived earlier");

        // Fresh store (simulating a process restart) over the same backend.
        let store = RecallStore::with_persistence(backend.clone());
        let got = store
            .get(conv)
            .expect("archive should hydrate from backend");
        assert_eq!(got.as_str(), "archived earlier");
    }

    #[test]
    fn append_concatenates_onto_hydrated_archive() {
        let backend = Arc::new(MockPersistence::default());
        let conv = Uuid::new_v4();
        backend.upsert(conv, "old");

        let store = RecallStore::with_persistence(backend.clone());
        store.append(conv, "new");
        assert_eq!(store.get(conv).unwrap().as_str(), "old\n\nnew");
        assert_eq!(
            backend.rows.lock().unwrap().get(&conv).unwrap(),
            "old\n\nnew"
        );
    }

    #[test]
    fn hydrate_caches_so_repeat_reads_dont_reload() {
        let backend = Arc::new(MockPersistence::default());
        let conv = Uuid::new_v4();
        backend.upsert(conv, "x");
        let store = RecallStore::with_persistence(backend.clone());
        store.get(conv);
        store.get(conv);
        // Only the first miss should touch the backend.
        assert_eq!(*backend.loads.lock().unwrap(), 1);
    }

    #[test]
    fn clear_keeps_durable_archive_purge_removes_it() {
        let backend = Arc::new(MockPersistence::default());
        let store = RecallStore::with_persistence(backend.clone());
        let conv = Uuid::new_v4();
        store.append(conv, "data");

        // clear() is cache-only: durable row survives and re-hydrates.
        store.clear(conv);
        assert!(backend.rows.lock().unwrap().contains_key(&conv));
        assert_eq!(store.get(conv).unwrap().as_str(), "data");

        // purge() removes both.
        store.purge(conv);
        assert!(!backend.rows.lock().unwrap().contains_key(&conv));
        assert!(store.get(conv).is_none());
    }
}
