//! A stable fingerprint of the tools array a request declares, logged once
//! per request against the conversation that sent it.
//!
//! Chat templates render the tools array into the front of the prompt, so a
//! request whose array differs from the previous one in the same
//! conversation throws away the cached prefix and pays a full re-prefill
//! (plan `docs/plans/control-layer-and-worker-fleet.md`, sections 2 and 12:
//! 6.5 s on gemma4:26b, 35 s on qwen3.8:27b at 7K tokens). The plan's rule
//! that the tool block is fixed for a run is only checkable if every request
//! says which block it carried and for which conversation, so providers call
//! [`ToolBlockTracker::observe`] with a [`fingerprint`] of the tools array they
//! send and log one `tool block sent` line per request
//! (`rustykrab_providers::log_tool_block`):
//!
//! ```text
//! INFO rustykrab_providers::tool_block: tool block sent provider="ollama"
//!      conversation_id=<uuid> trace_id=<uuid> tool_block=9f0c3e5a1b2d4c6e
//!      num_tools=13 tool_tokens=1540 changed=false
//! ```
//!
//! The comparison is per conversation. A provider-wide "last request" cannot
//! tell a mid-run load from three runs interleaving on one model, which is
//! what 31 of 46 logged changes on the live daemon turned out to be.
//! Requests outside any runner (the harness router, title generation) log
//! `conversation_id=-` and are never compared.
//!
//! The fingerprint is FNV-1a 64 over the serialized JSON, not `std`'s
//! `DefaultHasher`, whose output is not promised to survive a toolchain
//! update: a fingerprint read out of last month's log has to mean the same
//! bytes as one read today.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::Serialize;
use uuid::Uuid;

use crate::active_tools::with_session_context;
use crate::types::ToolSchema;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Conversations whose last fingerprint is remembered. Past this an
/// arbitrary entry is forgotten, so a long-lived daemon stays bounded; a
/// forgotten conversation's next request is simply not compared.
const TRACKED_CONVERSATIONS: usize = 1024;

/// FNV-1a over bytes as they are written, so a value is fingerprinted by
/// serializing it once and never materializing the JSON.
struct Fnv(u64);

impl std::io::Write for Fnv {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        for byte in buf {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(FNV_PRIME);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Fingerprint of a value as serialized JSON: the tools array in the
/// provider's own wire types, so any change to a name, description or
/// parameter moves it. `0` is never returned, so callers can use it for
/// "nothing sent yet".
pub fn fingerprint<T: Serialize + ?Sized>(value: &T) -> u64 {
    let mut hasher = Fnv(FNV_OFFSET);
    // Serializing to an in-memory writer fails only for a value that cannot
    // be serialized at all; hash what was written.
    let _ = serde_json::to_writer(&mut hasher, value);
    hasher.0 | 1
}

/// Fingerprint of the core tool schemas, for providers that send them as
/// they are.
pub fn fingerprint_schemas(tools: &[ToolSchema]) -> u64 {
    fingerprint(tools)
}

/// A fingerprint as it appears in the log: sixteen lowercase hex digits.
pub fn hex(fingerprint: u64) -> String {
    format!("{fingerprint:016x}")
}

/// What one request's tool block was, for whom, and whether it differs
/// from that conversation's previous request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolBlockObservation {
    pub fingerprint: u64,
    pub conversation_id: Option<Uuid>,
    /// The previous fingerprint of the same conversation, when it differs
    /// from this one. `None` on a conversation's first request, on an
    /// unchanged block, and outside any conversation.
    pub changed_from: Option<u64>,
}

impl ToolBlockObservation {
    pub fn changed(&self) -> bool {
        self.changed_from.is_some()
    }
}

/// The last tool block each conversation sent, per provider instance.
#[derive(Debug, Default)]
pub struct ToolBlockTracker {
    last: Mutex<HashMap<Uuid, u64>>,
}

impl ToolBlockTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a request's tool block against the conversation of the
    /// runner it is sent from (the task-local [`SESSION_TOOL_CONTEXT`]).
    /// The provider logs the result; see the module docs for the line.
    ///
    /// [`SESSION_TOOL_CONTEXT`]: crate::active_tools::SESSION_TOOL_CONTEXT
    pub fn observe(&self, fingerprint: u64) -> ToolBlockObservation {
        let conversation_id = with_session_context(|ctx| ctx.conversation_id);
        self.observe_for(conversation_id, fingerprint)
    }

    /// [`observe`](Self::observe) for an explicit conversation.
    pub fn observe_for(
        &self,
        conversation_id: Option<Uuid>,
        fingerprint: u64,
    ) -> ToolBlockObservation {
        let Some(id) = conversation_id else {
            return ToolBlockObservation {
                fingerprint,
                conversation_id: None,
                changed_from: None,
            };
        };
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if last.len() >= TRACKED_CONVERSATIONS && !last.contains_key(&id) {
            if let Some(evict) = last.keys().next().copied() {
                last.remove(&evict);
            }
        }
        let previous = last.insert(id, fingerprint);
        ToolBlockObservation {
            fingerprint,
            conversation_id: Some(id),
            changed_from: previous.filter(|p| *p != fingerprint),
        }
    }

    /// Forget a conversation, e.g. when its runner ends for good.
    pub fn forget(&self, conversation_id: Uuid) {
        self.last
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&conversation_id);
    }
}

/// An id for a log field: the id, or `-` when there is none.
pub fn display_id(id: Option<Uuid>) -> String {
    id.map_or_else(|| "-".to_string(), |id| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::active_tools::{ActiveToolsRegistry, SessionToolContext, SESSION_TOOL_CONTEXT};
    use crate::capability::CapabilitySet;
    use crate::recall::RecallStore;
    use crate::todo::TodoStore;
    use serde_json::json;
    use std::sync::Arc;

    fn schema(name: &str, description: &str, parameters: serde_json::Value) -> ToolSchema {
        ToolSchema {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }

    fn block() -> Vec<ToolSchema> {
        vec![
            schema(
                "tools_list",
                "Search the catalog.",
                json!({"type": "object"}),
            ),
            schema(
                "get_weather",
                "Get the current weather for a city.",
                json!({"type": "object", "properties": {"city": {"type": "string"}}}),
            ),
        ]
    }

    #[test]
    fn a_fingerprint_is_stable_and_covers_every_byte_of_the_block() {
        let base = fingerprint_schemas(&block());
        assert_eq!(
            base,
            fingerprint_schemas(&block()),
            "same bytes, same print"
        );
        assert_ne!(base, 0);
        // Pinned: FNV-1a is specified, so a print read from an old log keeps
        // its meaning across toolchains. A change here is a format change.
        assert_eq!(hex(fingerprint(&json!([]))), "09612b07b5ecb5a5");

        let mut renamed = block();
        renamed[1].name = "get_forecast".into();
        let mut described = block();
        described[1].description.push('!');
        let mut reparametered = block();
        reparametered[1].parameters = json!({"type": "object"});
        let mut reordered = block();
        reordered.reverse();
        let mut grown = block();
        grown.push(schema("task_complete", "Finish.", json!({})));
        for other in [renamed, described, reparametered, reordered, grown] {
            assert_ne!(fingerprint_schemas(&other), base);
        }
        assert_eq!(hex(base).len(), 16);
    }

    #[test]
    fn changes_are_compared_within_a_conversation_only() {
        let tracker = ToolBlockTracker::new();
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());

        let first = tracker.observe_for(Some(a), 11);
        assert!(
            !first.changed(),
            "a conversation's first block is no change"
        );
        // Another conversation interleaving with a different block is not a
        // change for either of them: the case the provider-wide comparison
        // could not tell from a mid-run load.
        assert!(!tracker.observe_for(Some(b), 22).changed());
        assert!(!tracker.observe_for(Some(a), 11).changed());
        assert!(!tracker.observe_for(Some(b), 22).changed());

        let moved = tracker.observe_for(Some(a), 33);
        assert_eq!(moved.changed_from, Some(11));
        assert!(!tracker.observe_for(Some(a), 33).changed());

        // Outside a conversation nothing is compared or remembered.
        assert!(!tracker.observe_for(None, 44).changed());
        assert!(!tracker.observe_for(None, 55).changed());

        tracker.forget(a);
        assert!(!tracker.observe_for(Some(a), 66).changed());
    }

    #[test]
    fn the_tracker_stays_bounded() {
        let tracker = ToolBlockTracker::new();
        for _ in 0..(TRACKED_CONVERSATIONS + 10) {
            tracker.observe_for(Some(Uuid::new_v4()), 7);
        }
        assert!(tracker.last.lock().unwrap().len() <= TRACKED_CONVERSATIONS);
    }

    #[tokio::test]
    async fn observe_attributes_a_request_to_the_runners_conversation() {
        let conversation_id = Uuid::new_v4();
        let ctx = SessionToolContext {
            conversation_id,
            capabilities: Arc::new(CapabilitySet::default_safe()),
            all_tools: Arc::new(Vec::new()),
            active_tools: Arc::new(ActiveToolsRegistry::new()),
            recall: Arc::new(RecallStore::new()),
            todos: Arc::new(TodoStore::new()),
        };
        let tracker = ToolBlockTracker::new();
        let seen = SESSION_TOOL_CONTEXT
            .scope(ctx, async { tracker.observe(9) })
            .await;
        assert_eq!(seen.conversation_id, Some(conversation_id));
        assert_eq!(tracker.observe(9).conversation_id, None);
    }
}
