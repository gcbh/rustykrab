use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};

use crate::{ProjectError, Result};

/// A stable link to an existing conversation message. Message text is not
/// duplicated in the project model.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct MessageRef {
    pub conversation_id: String,
    pub message_id: String,
}

impl MessageRef {
    pub fn new(conversation_id: impl Into<String>, message_id: impl Into<String>) -> Result<Self> {
        let value = Self {
            conversation_id: conversation_id.into(),
            message_id: message_id.into(),
        };
        if value.conversation_id.trim().is_empty() {
            return Err(ProjectError::EmptyField {
                field: "conversation_id",
            });
        }
        if value.message_id.trim().is_empty() {
            return Err(ProjectError::EmptyField {
                field: "message_id",
            });
        }
        Ok(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvenanceClassification {
    UserStated,
    RepositoryObserved,
    ExperimentObserved,
    ResearchFinding,
    AgentInferred,
    AgentProposed,
    SystemObserved,
    Custom(String),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProvenanceSource {
    ConversationMessage {
        conversation_id: String,
        message_id: String,
    },
    Repository {
        repository: String,
        revision: String,
        paths: Vec<String>,
        evidence_hash: String,
    },
    Experiment {
        trace_id: String,
        evidence_hash: Option<String>,
    },
    Research {
        uri: String,
        evidence_hash: Option<String>,
    },
    DeliveryEvidence {
        delivery_id: String,
        evidence_id: String,
    },
    Manual {
        reference: String,
    },
    Custom {
        source_type: String,
        reference: String,
    },
}

/// Confidence represented as basis points avoids floating-point instability in
/// revision identities.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct Confidence(u16);

impl Confidence {
    pub const CERTAIN: Self = Self(10_000);

    pub fn new(basis_points: u16) -> Result<Self> {
        if basis_points > 10_000 {
            return Err(ProjectError::InvalidConfidence(basis_points));
        }
        Ok(Self(basis_points))
    }

    pub const fn basis_points(self) -> u16 {
        self.0
    }
}

impl<'de> Deserialize<'de> for Confidence {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let basis_points = u16::deserialize(deserializer)?;
        Self::new(basis_points).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Freshness {
    pub observed_at: DateTime<Utc>,
    pub valid_until: Option<DateTime<Utc>>,
    pub policy: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Provenance {
    pub classification: ProvenanceClassification,
    pub source: ProvenanceSource,
    pub recorded_at: DateTime<Utc>,
    pub confidence: Option<Confidence>,
    pub freshness: Option<Freshness>,
}

impl Provenance {
    pub fn conversation(
        classification: ProvenanceClassification,
        message: &MessageRef,
        recorded_at: DateTime<Utc>,
    ) -> Self {
        Self {
            classification,
            source: ProvenanceSource::ConversationMessage {
                conversation_id: message.conversation_id.clone(),
                message_id: message.message_id.clone(),
            },
            recorded_at,
            confidence: None,
            freshness: None,
        }
    }
}
