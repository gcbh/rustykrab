use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    EdgeId, JudgmentPolicy, MessageRef, NodeData, NodeId, PlanEdgeDraft, PlanNodeDraft, ProjectId,
    ProjectStatus, Provenance, RevisionAuthor, RevisionId,
};

/// A complete atomic edit against one immutable parent revision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanChangeSet {
    pub request_id: String,
    pub project_id: ProjectId,
    pub parent_revision: RevisionId,
    pub summary: String,
    pub author: RevisionAuthor,
    pub source_message: Option<MessageRef>,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_patch: Option<ProjectPatch>,
    pub changes: Vec<PlanChange>,
}

impl PlanChangeSet {
    pub fn new(
        request_id: impl Into<String>,
        project_id: ProjectId,
        parent_revision: RevisionId,
        summary: impl Into<String>,
        author: RevisionAuthor,
        created_at: DateTime<Utc>,
        changes: Vec<PlanChange>,
    ) -> Self {
        Self {
            request_id: request_id.into(),
            project_id,
            parent_revision,
            summary: summary.into(),
            author,
            source_message: None,
            created_at,
            project_patch: None,
            changes,
        }
    }

    pub fn with_source_message(mut self, source_message: MessageRef) -> Self {
        self.source_message = Some(source_message);
        self
    }

    pub fn with_project_patch(mut self, project_patch: ProjectPatch) -> Self {
        self.project_patch = Some(project_patch);
        self
    }
}

/// Metadata edits that are revisioned with the graph. Double options allow a
/// caller to distinguish "leave unchanged" from "clear this optional value".
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectPatch {
    pub provenance: Vec<Provenance>,
    pub title: Option<String>,
    pub status: Option<ProjectStatus>,
    pub repository_id: Option<Option<String>>,
    pub canonical_conversation_id: Option<Option<String>>,
    pub judgment_policy: Option<JudgmentPolicy>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum PlanChange {
    AddNode {
        node: PlanNodeDraft,
    },
    UpdateNode {
        node_id: NodeId,
        patch: PlanNodePatch,
        provenance: Vec<Provenance>,
    },
    SupersedeNode {
        node_id: NodeId,
        replacement: PlanNodeDraft,
    },
    AddEdge {
        edge: PlanEdgeDraft,
    },
    RemoveEdge {
        edge_id: EdgeId,
        provenance: Vec<Provenance>,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanNodePatch {
    pub title: Option<String>,
    pub body: Option<String>,
    pub data: Option<NodeData>,
}
