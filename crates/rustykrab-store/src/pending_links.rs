//! Credential links held back until the turn that minted them has spoken.
//!
//! The link used to be handed to the model in the tool result, and the
//! model was asked to relay it. That makes an LLM responsible for copying
//! 64 hex characters exactly, and it does not reliably manage it —
//! observed against gemma4:26b relaying 55 of 64, which the user then
//! opens to a page saying "Link expired". Deliberately indistinguishable
//! from a real expiry, so nobody can tell a typo from a timeout.
//!
//! Parking the link here instead means the model never sees it, so it
//! cannot truncate, paraphrase, or omit it — and the URL never enters the
//! context window, which matters because a live credential-capture link
//! in a transcript is a capture form anyone reading the history can open
//! for as long as it lives.
//!
//! In memory on purpose. The plaintext token exists only between minting
//! and delivery; only its hash is ever written down. A link that does not
//! survive a restart is a link an operator has to ask for again, which is
//! the correct outcome — the alternative is storing a live credential
//! capture URL on disk.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use uuid::Uuid;

/// Links awaiting delivery, keyed by the conversation that asked.
#[derive(Clone, Default)]
pub struct PendingLinks {
    inner: Arc<Mutex<HashMap<Uuid, Vec<String>>>>,
}

impl PendingLinks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a link for delivery once the current turn finishes.
    ///
    /// Keyed by conversation rather than request id: the deliverer knows
    /// which conversation just spoke, and nothing else.
    pub fn push(&self, conversation_id: Uuid, link: String) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(conversation_id)
            .or_default()
            .push(link);
    }

    /// Take everything queued for a conversation, leaving it empty.
    ///
    /// Taking rather than reading: a link delivered twice is a second
    /// message the user has to reason about, and the token only works
    /// once anyway.
    pub fn take(&self, conversation_id: Uuid) -> Vec<String> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&conversation_id)
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_is_delivered_once_and_then_gone() {
        let links = PendingLinks::new();
        let conv = Uuid::new_v4();
        links.push(conv, "https://host/c/abc".into());

        assert_eq!(links.take(conv), vec!["https://host/c/abc".to_string()]);
        assert!(
            links.take(conv).is_empty(),
            "a second delivery would be a duplicate message for a single-use token"
        );
    }

    #[test]
    fn conversations_do_not_see_each_others_links() {
        let links = PendingLinks::new();
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        links.push(a, "https://host/c/aaa".into());
        links.push(b, "https://host/c/bbb".into());

        assert_eq!(links.take(a), vec!["https://host/c/aaa".to_string()]);
        assert_eq!(links.take(b), vec!["https://host/c/bbb".to_string()]);
    }

    /// An agent that asks for two credentials in one turn owes the user
    /// two links, in the order it asked for them.
    #[test]
    fn several_links_in_one_turn_are_kept_in_order() {
        let links = PendingLinks::new();
        let conv = Uuid::new_v4();
        links.push(conv, "first".into());
        links.push(conv, "second".into());
        assert_eq!(
            links.take(conv),
            vec!["first".to_string(), "second".to_string()]
        );
    }

    #[test]
    fn a_conversation_that_asked_for_nothing_yields_nothing() {
        assert!(PendingLinks::new().take(Uuid::new_v4()).is_empty());
    }
}
