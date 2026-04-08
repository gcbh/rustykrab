use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::capability::CapabilitySet;

/// An isolated session binding a conversation to a set of capabilities.
///
/// Every active conversation runs within a Session, which constrains
/// what tools and resources the agent can access. This is the primary
/// mechanism for preventing cross-session data leakage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// Unique session ID.
    pub id: Uuid,
    /// The conversation this session is bound to.
    pub conversation_id: Uuid,
    /// The capabilities granted to this session.
    pub capabilities: CapabilitySet,
    /// When this session was created.
    pub created_at: DateTime<Utc>,
    /// When this session expires (None = no expiry).
    pub expires_at: Option<DateTime<Utc>>,
}

impl Session {
    /// Create a new session with default-safe capabilities.
    pub fn new(conversation_id: Uuid) -> Self {
        Self {
            id: Uuid::new_v4(),
            conversation_id,
            capabilities: CapabilitySet::default_safe(),
            created_at: Utc::now(),
            expires_at: None,
        }
    }

    /// Create a session with explicit capabilities.
    pub fn with_capabilities(conversation_id: Uuid, capabilities: CapabilitySet) -> Self {
        Self {
            id: Uuid::new_v4(),
            conversation_id,
            capabilities,
            created_at: Utc::now(),
            expires_at: None,
        }
    }

    /// Set an expiration time on this session.
    pub fn with_expiry(mut self, expires_at: DateTime<Utc>) -> Self {
        self.expires_at = Some(expires_at);
        self
    }

    /// Check whether this session has expired.
    pub fn is_expired(&self) -> bool {
        self.expires_at
            .map(|exp| Utc::now() > exp)
            .unwrap_or(false)
    }
}
