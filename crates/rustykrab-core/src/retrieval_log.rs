//! Which memories were surfaced into a conversation.
//!
//! Credit assignment needs a link that does not exist naturally: the
//! retrieval path knows *what was recalled* but not how the turn went, and
//! the run-completion path knows *how the turn went* but not what was
//! recalled. Without something joining them, an outcome can only say "that
//! turn went badly", which is not actionable.
//!
//! This log is that join. Retrieval writes memory ids against a
//! conversation; run completion drains them and attaches them to the
//! outcome record. It is deliberately in-process and lossy — it is an
//! attribution hint, not a ledger. Losing entries on restart costs some
//! attribution fidelity and nothing else.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use uuid::Uuid;

/// Memory ids retained per conversation. Generous enough to cover a turn's
/// worth of recalls, small enough that attribution stays meaningful — if
/// two hundred memories were in play, blaming any one of them is noise.
const MAX_IDS_PER_CONVERSATION: usize = 64;

/// Conversations tracked at once. Bounds the map for a process that may run
/// for weeks across many conversations.
const MAX_CONVERSATIONS: usize = 512;

/// In-process record of which memories were surfaced for each conversation.
///
/// Cheap to clone; all clones share one table.
#[derive(Clone, Default)]
pub struct RetrievalLog {
    inner: Arc<Mutex<HashMap<Uuid, Vec<Uuid>>>>,
}

impl RetrievalLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Acquire the table, recovering from a poisoned lock rather than
    /// propagating the panic. An attribution hint is never worth taking the
    /// process down for.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, Vec<Uuid>>> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Note that `memory_ids` were surfaced for `conversation_id`.
    ///
    /// Ids already present are not re-added: a memory recalled three times
    /// in one turn contributed once, and counting it three times would let
    /// repeated searches inflate its share of the credit.
    pub fn record(&self, conversation_id: Uuid, memory_ids: impl IntoIterator<Item = Uuid>) {
        let mut map = self.lock();

        // Evict an arbitrary other conversation when full. Arbitrary is
        // acceptable: this is a hint, and the alternative (tracking
        // recency) costs more than the precision is worth.
        if map.len() >= MAX_CONVERSATIONS && !map.contains_key(&conversation_id) {
            if let Some(victim) = map.keys().next().copied() {
                map.remove(&victim);
            }
        }

        let entry = map.entry(conversation_id).or_default();
        for id in memory_ids {
            if entry.len() >= MAX_IDS_PER_CONVERSATION {
                break;
            }
            if !entry.contains(&id) {
                entry.push(id);
            }
        }
    }

    /// Take the ids recorded for `conversation_id`, clearing them.
    ///
    /// Draining is what keeps attribution honest across turns: each
    /// outcome is credited to the memories recalled since the last outcome,
    /// not to everything the conversation has ever seen.
    pub fn take(&self, conversation_id: Uuid) -> Vec<Uuid> {
        self.lock().remove(&conversation_id).unwrap_or_default()
    }

    /// Read the ids without clearing them.
    pub fn peek(&self, conversation_id: Uuid) -> Vec<Uuid> {
        self.lock()
            .get(&conversation_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Drop a conversation's entry, e.g. when the conversation ends.
    pub fn forget(&self, conversation_id: Uuid) {
        self.lock().remove(&conversation_id);
    }

    /// Number of conversations currently tracked.
    pub fn tracked_conversations(&self) -> usize {
        self.lock().len()
    }
}

impl std::fmt::Debug for RetrievalLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetrievalLog")
            .field("conversations", &self.tracked_conversations())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_drains_per_conversation() {
        let log = RetrievalLog::new();
        let conv = Uuid::new_v4();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();

        log.record(conv, [a, b]);
        assert_eq!(log.peek(conv), vec![a, b]);

        let taken = log.take(conv);
        assert_eq!(taken, vec![a, b]);
        // Draining clears, so the next turn starts clean.
        assert!(log.take(conv).is_empty());
    }

    #[test]
    fn repeated_recalls_count_once() {
        let log = RetrievalLog::new();
        let conv = Uuid::new_v4();
        let a = Uuid::new_v4();

        log.record(conv, [a]);
        log.record(conv, [a, a]);
        assert_eq!(log.take(conv), vec![a]);
    }

    #[test]
    fn conversations_are_isolated() {
        let log = RetrievalLog::new();
        let (c1, c2) = (Uuid::new_v4(), Uuid::new_v4());
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());

        log.record(c1, [a]);
        log.record(c2, [b]);
        assert_eq!(log.take(c1), vec![a]);
        assert_eq!(log.take(c2), vec![b]);
    }

    #[test]
    fn per_conversation_ids_are_bounded() {
        let log = RetrievalLog::new();
        let conv = Uuid::new_v4();
        let ids: Vec<Uuid> = (0..MAX_IDS_PER_CONVERSATION * 2)
            .map(|_| Uuid::new_v4())
            .collect();

        log.record(conv, ids);
        assert_eq!(log.peek(conv).len(), MAX_IDS_PER_CONVERSATION);
    }

    #[test]
    fn conversation_count_is_bounded() {
        let log = RetrievalLog::new();
        for _ in 0..MAX_CONVERSATIONS + 50 {
            log.record(Uuid::new_v4(), [Uuid::new_v4()]);
        }
        assert!(log.tracked_conversations() <= MAX_CONVERSATIONS);
    }

    #[test]
    fn forget_removes_entry() {
        let log = RetrievalLog::new();
        let conv = Uuid::new_v4();
        log.record(conv, [Uuid::new_v4()]);
        log.forget(conv);
        assert!(log.peek(conv).is_empty());
    }
}
