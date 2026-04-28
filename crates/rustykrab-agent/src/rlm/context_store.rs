//! Per-conversation context store for the RLM REPL tools.
//!
//! The four context tools (`context_info`, `context_peek`,
//! `context_search`, `context_set`) operate on a string blob keyed by
//! the active conversation's id. The blob lives outside the prompt so
//! a small model can explore it via tool calls instead of paying the
//! token cost of having it in every turn.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use uuid::Uuid;

/// Maximum bytes a single context blob may hold. Caps memory growth from
/// runaway `context_set` calls; the model gets an error if it tries to
/// stash something larger.
pub const MAX_CONTEXT_BYTES: usize = 4 * 1024 * 1024;

/// Thread-safe map of conversation id → context blob.
#[derive(Debug, Default)]
pub struct ContextStore {
    inner: RwLock<HashMap<Uuid, Arc<String>>>,
}

impl ContextStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the context blob for `conv_id`, replacing any existing entry.
    /// Returns the byte length stored.
    pub fn set(&self, conv_id: Uuid, text: String) -> usize {
        let len = text.len();
        let mut map = self.inner.write().expect("ContextStore poisoned");
        map.insert(conv_id, Arc::new(text));
        len
    }

    /// Look up the context blob for `conv_id`.
    pub fn get(&self, conv_id: Uuid) -> Option<Arc<String>> {
        let map = self.inner.read().expect("ContextStore poisoned");
        map.get(&conv_id).cloned()
    }

    /// Remove the entry for `conv_id`. Returns `true` if one existed.
    pub fn clear(&self, conv_id: Uuid) -> bool {
        let mut map = self.inner.write().expect("ContextStore poisoned");
        map.remove(&conv_id).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_and_get_roundtrip() {
        let store = ContextStore::new();
        let id = Uuid::new_v4();
        let n = store.set(id, "hello".into());
        assert_eq!(n, 5);
        assert_eq!(store.get(id).as_deref().map(String::as_str), Some("hello"));
    }

    #[test]
    fn entries_are_per_conversation() {
        let store = ContextStore::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        store.set(a, "for a".into());
        store.set(b, "for b".into());
        assert_eq!(store.get(a).as_deref().map(String::as_str), Some("for a"));
        assert_eq!(store.get(b).as_deref().map(String::as_str), Some("for b"));
    }

    #[test]
    fn clear_removes_entry() {
        let store = ContextStore::new();
        let id = Uuid::new_v4();
        store.set(id, "x".into());
        assert!(store.clear(id));
        assert!(store.get(id).is_none());
        assert!(!store.clear(id));
    }
}
