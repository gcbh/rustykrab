use thiserror::Error;

use crate::{EdgeId, NodeId};

pub type Result<T> = std::result::Result<T, ProjectError>;

/// A rejected project-domain operation. Applying a change set is transactional,
/// so any error leaves the source revision untouched.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ProjectError {
    #[error("{field} must not be empty")]
    EmptyField { field: &'static str },
    #[error("confidence basis points must be between 0 and 10,000, got {0}")]
    InvalidConfidence(u16),
    #[error("plan change {change_index} has no provenance")]
    MissingProvenance { change_index: usize },
    #[error("plan change {change_index} has invalid provenance: {reason}")]
    InvalidProvenance { change_index: usize, reason: String },
    #[error("project metadata change has no provenance")]
    MissingProjectProvenance,
    #[error("node {0} already exists")]
    DuplicateNode(NodeId),
    #[error("node {0} does not exist")]
    NodeNotFound(NodeId),
    #[error("node {0} has already been superseded")]
    NodeAlreadySuperseded(NodeId),
    #[error("edge {0} already exists")]
    DuplicateEdge(EdgeId),
    #[error("edge {0} does not exist")]
    EdgeNotFound(EdgeId),
    #[error("edge {0} has already been retired")]
    EdgeAlreadyRetired(EdgeId),
    #[error("edge {edge_id} cannot connect node {node_id} to itself")]
    SelfReferentialEdge { edge_id: EdgeId, node_id: NodeId },
    #[error("edge {edge_id} has invalid relationship: {reason}")]
    InvalidRelationship { edge_id: EdgeId, reason: String },
    #[error("node {node_id} has data incompatible with kind {kind}")]
    IncompatibleNodeData { node_id: NodeId, kind: String },
    #[error("node {node_id} references missing node {referenced_id} in {field}")]
    MissingNodeReference {
        node_id: NodeId,
        referenced_id: NodeId,
        field: &'static str,
    },
    #[error("node {node_id} has an invalid state: {reason}")]
    InvalidNodeState { node_id: NodeId, reason: String },
    #[error("replacement node kind must match the superseded node kind")]
    SupersessionKindMismatch,
    #[error("change set project {actual} does not match snapshot project {expected}")]
    ProjectMismatch { expected: String, actual: String },
    #[error("change set parent {actual} does not match current revision {expected}")]
    ParentRevisionMismatch { expected: String, actual: String },
    #[error("revision sequence overflow")]
    RevisionSequenceOverflow,
    #[error("revision time {revision_time} is before the project's latest update {updated_at}")]
    RevisionTimeBeforeProject {
        revision_time: String,
        updated_at: String,
    },
    #[error("cannot compare revision sequence {from_sequence} to older sequence {to_sequence}")]
    InvalidRevisionComparison {
        from_sequence: u64,
        to_sequence: u64,
    },
    #[error("failed to produce canonical project content: {0}")]
    CanonicalSerialization(String),
}
