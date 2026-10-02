use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{
    EdgeId, EdgeRelation, JudgmentPolicy, MessageRef, NodeData, NodeId, NodeKind, PlanChange,
    PlanChangeSet, PlanEdge, PlanNode, Project, ProjectError, ProjectId, ProjectStatus, Provenance,
    Result, RevisionId,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevisionAuthor {
    User,
    Agent,
    System,
    Delivery,
    Custom(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateProject {
    pub request_id: String,
    pub project_id: ProjectId,
    pub repository_id: Option<String>,
    pub title: String,
    pub status: ProjectStatus,
    pub judgment_policy: JudgmentPolicy,
    pub canonical_conversation_id: Option<String>,
    pub summary: String,
    pub author: RevisionAuthor,
    pub source_message: Option<MessageRef>,
    /// Provenance specific to project metadata changed by this revision.
    pub project_provenance: Vec<Provenance>,
    pub created_at: DateTime<Utc>,
    pub initial_changes: Vec<PlanChange>,
}

impl CreateProject {
    pub fn new(
        request_id: impl Into<String>,
        project_id: ProjectId,
        title: impl Into<String>,
        author: RevisionAuthor,
        created_at: DateTime<Utc>,
    ) -> Self {
        Self {
            request_id: request_id.into(),
            project_id,
            repository_id: None,
            title: title.into(),
            status: ProjectStatus::Active,
            judgment_policy: JudgmentPolicy::default(),
            canonical_conversation_id: None,
            summary: "Create project".to_owned(),
            author,
            source_message: None,
            project_provenance: Vec::new(),
            created_at,
            initial_changes: Vec::new(),
        }
    }
}

/// One immutable, self-contained snapshot of the planning graph.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRevision {
    pub id: RevisionId,
    pub request_id: String,
    pub project_id: ProjectId,
    pub parent_revision: Option<RevisionId>,
    pub sequence: u64,
    pub author: RevisionAuthor,
    pub summary: String,
    pub source_message: Option<MessageRef>,
    /// Provenance specific to project metadata changed by this revision.
    pub project_provenance: Vec<Provenance>,
    pub created_at: DateTime<Utc>,
    pub nodes: BTreeMap<NodeId, PlanNode>,
    pub edges: BTreeMap<EdgeId, PlanEdge>,
}

impl ProjectRevision {
    fn apply(&self, project: &Project, change_set: PlanChangeSet) -> Result<Self> {
        if self.project_id != project.id {
            return Err(ProjectError::ProjectMismatch {
                expected: self.project_id.to_string(),
                actual: project.id.to_string(),
            });
        }
        if change_set.project_id != project.id {
            return Err(ProjectError::ProjectMismatch {
                expected: project.id.to_string(),
                actual: change_set.project_id.to_string(),
            });
        }
        if change_set.parent_revision != self.id {
            return Err(ProjectError::ParentRevisionMismatch {
                expected: self.id.to_string(),
                actual: change_set.parent_revision.to_string(),
            });
        }
        validate_text("request_id", &change_set.request_id)?;
        validate_text("summary", &change_set.summary)?;
        if let Some(message) = &change_set.source_message {
            validate_message_ref(message)?;
        }

        let sequence = self
            .sequence
            .checked_add(1)
            .ok_or(ProjectError::RevisionSequenceOverflow)?;
        let mut nodes = self.nodes.clone();
        let mut edges = self.edges.clone();

        apply_changes(sequence, &change_set.changes, &mut nodes, &mut edges)?;
        validate_graph(&nodes, &edges)?;

        let mut revision = Self {
            id: RevisionId::from_hash(String::new()),
            request_id: change_set.request_id,
            project_id: project.id,
            parent_revision: Some(self.id.clone()),
            sequence,
            author: change_set.author,
            summary: change_set.summary,
            source_message: change_set.source_message,
            project_provenance: change_set
                .project_patch
                .map(|patch| patch.provenance)
                .unwrap_or_default(),
            created_at: change_set.created_at,
            nodes,
            edges,
        };
        revision.id = canonical_revision_id(project, &revision)?;
        Ok(revision)
    }

    pub fn current_nodes(&self) -> impl Iterator<Item = &PlanNode> {
        self.nodes.values().filter(|node| node.is_current())
    }

    pub fn current_edges(&self) -> impl Iterator<Item = &PlanEdge> {
        self.edges.values().filter(|edge| edge.is_current())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectSnapshot {
    pub project: Project,
    pub revision: ProjectRevision,
}

impl ProjectSnapshot {
    pub fn create(command: CreateProject) -> Result<Self> {
        validate_text("request_id", &command.request_id)?;
        validate_text("project title", &command.title)?;
        validate_text("summary", &command.summary)?;
        if let Some(repository_id) = &command.repository_id {
            validate_text("repository_id", repository_id)?;
        }
        if let Some(conversation_id) = &command.canonical_conversation_id {
            validate_text("canonical_conversation_id", conversation_id)?;
        }
        if let Some(message) = &command.source_message {
            validate_message_ref(message)?;
        }
        if !command.project_provenance.is_empty() {
            require_provenance(command.initial_changes.len(), &command.project_provenance)?;
        }

        let project = Project {
            id: command.project_id,
            repository_id: command.repository_id,
            title: command.title,
            status: command.status,
            judgment_policy: command.judgment_policy,
            canonical_conversation_id: command.canonical_conversation_id,
            created_at: command.created_at,
            updated_at: command.created_at,
        };
        let mut nodes = BTreeMap::new();
        let mut edges = BTreeMap::new();
        apply_changes(0, &command.initial_changes, &mut nodes, &mut edges)?;
        validate_graph(&nodes, &edges)?;

        let mut revision = ProjectRevision {
            id: RevisionId::from_hash(String::new()),
            request_id: command.request_id,
            project_id: project.id,
            parent_revision: None,
            sequence: 0,
            author: command.author,
            summary: command.summary,
            source_message: command.source_message,
            project_provenance: command.project_provenance,
            created_at: command.created_at,
            nodes,
            edges,
        };
        revision.id = canonical_revision_id(&project, &revision)?;
        Ok(Self { project, revision })
    }

    pub fn apply(&self, change_set: PlanChangeSet) -> Result<Self> {
        let mut project = self.project.clone();
        if change_set.created_at < project.updated_at {
            return Err(ProjectError::RevisionTimeBeforeProject {
                revision_time: change_set.created_at.to_rfc3339(),
                updated_at: project.updated_at.to_rfc3339(),
            });
        }
        if let Some(patch) = &change_set.project_patch {
            if patch.provenance.is_empty() {
                return Err(ProjectError::MissingProjectProvenance);
            }
            require_provenance(change_set.changes.len(), &patch.provenance)?;
            if let Some(title) = &patch.title {
                validate_text("project title", title)?;
                project.title.clone_from(title);
            }
            if let Some(status) = &patch.status {
                project.status.clone_from(status);
            }
            if let Some(repository_id) = &patch.repository_id {
                if let Some(value) = repository_id {
                    validate_text("repository_id", value)?;
                }
                project.repository_id.clone_from(repository_id);
            }
            if let Some(conversation_id) = &patch.canonical_conversation_id {
                if let Some(value) = conversation_id {
                    validate_text("canonical_conversation_id", value)?;
                }
                project
                    .canonical_conversation_id
                    .clone_from(conversation_id);
            }
            if let Some(policy) = &patch.judgment_policy {
                project.judgment_policy.clone_from(policy);
            }
        }
        project.updated_at = change_set.created_at;
        let revision = self.revision.apply(&project, change_set)?;
        Ok(Self { project, revision })
    }
}

fn apply_changes(
    sequence: u64,
    changes: &[PlanChange],
    nodes: &mut BTreeMap<NodeId, PlanNode>,
    edges: &mut BTreeMap<EdgeId, PlanEdge>,
) -> Result<()> {
    for (change_index, change) in changes.iter().enumerate() {
        match change {
            PlanChange::AddNode { node } => {
                require_provenance(change_index, &node.provenance)?;
                if nodes.contains_key(&node.id) {
                    return Err(ProjectError::DuplicateNode(node.id));
                }
                validate_text("node title", &node.title)?;
                validate_node_data(node.id, &node.kind, &node.data)?;
                nodes.insert(
                    node.id,
                    PlanNode {
                        id: node.id,
                        kind: node.kind.clone(),
                        title: node.title.clone(),
                        body: node.body.clone(),
                        data: node.data.clone(),
                        provenance: node.provenance.clone(),
                        introduced_revision: sequence,
                        updated_revision: sequence,
                        superseded_revision: None,
                        superseded_by: None,
                    },
                );
            }
            PlanChange::UpdateNode {
                node_id,
                patch,
                provenance,
            } => {
                require_provenance(change_index, provenance)?;
                let node = current_node_mut(nodes, *node_id)?;
                if let Some(title) = &patch.title {
                    validate_text("node title", title)?;
                    node.title.clone_from(title);
                }
                if let Some(body) = &patch.body {
                    node.body.clone_from(body);
                }
                if let Some(data) = &patch.data {
                    validate_node_data(*node_id, &node.kind, data)?;
                    node.data.clone_from(data);
                }
                node.provenance.extend(provenance.iter().cloned());
                node.updated_revision = sequence;
            }
            PlanChange::SupersedeNode {
                node_id,
                replacement,
            } => {
                require_provenance(change_index, &replacement.provenance)?;
                if nodes.contains_key(&replacement.id) {
                    return Err(ProjectError::DuplicateNode(replacement.id));
                }
                let old_kind = current_node_mut(nodes, *node_id)?.kind.clone();
                if old_kind != replacement.kind {
                    return Err(ProjectError::SupersessionKindMismatch);
                }
                validate_text("node title", &replacement.title)?;
                validate_node_data(replacement.id, &replacement.kind, &replacement.data)?;

                let old = current_node_mut(nodes, *node_id)?;
                old.superseded_revision = Some(sequence);
                old.superseded_by = Some(replacement.id);
                old.updated_revision = sequence;
                nodes.insert(
                    replacement.id,
                    PlanNode {
                        id: replacement.id,
                        kind: replacement.kind.clone(),
                        title: replacement.title.clone(),
                        body: replacement.body.clone(),
                        data: replacement.data.clone(),
                        provenance: replacement.provenance.clone(),
                        introduced_revision: sequence,
                        updated_revision: sequence,
                        superseded_revision: None,
                        superseded_by: None,
                    },
                );
                for edge in edges.values_mut().filter(|edge| {
                    edge.is_current() && (edge.from == *node_id || edge.to == *node_id)
                }) {
                    edge.superseded_revision = Some(sequence);
                }
            }
            PlanChange::AddEdge { edge } => {
                require_provenance(change_index, &edge.provenance)?;
                if edges.contains_key(&edge.id) {
                    return Err(ProjectError::DuplicateEdge(edge.id));
                }
                validate_edge_draft(edge, nodes)?;
                edges.insert(
                    edge.id,
                    PlanEdge {
                        id: edge.id,
                        from: edge.from,
                        relation: edge.relation.clone(),
                        to: edge.to,
                        provenance: edge.provenance.clone(),
                        introduced_revision: sequence,
                        superseded_revision: None,
                    },
                );
            }
            PlanChange::RemoveEdge {
                edge_id,
                provenance,
            } => {
                require_provenance(change_index, provenance)?;
                let edge = edges
                    .get_mut(edge_id)
                    .ok_or(ProjectError::EdgeNotFound(*edge_id))?;
                if !edge.is_current() {
                    return Err(ProjectError::EdgeAlreadyRetired(*edge_id));
                }
                edge.provenance.extend(provenance.iter().cloned());
                edge.superseded_revision = Some(sequence);
            }
        }
    }
    Ok(())
}

fn require_provenance(change_index: usize, provenance: &[Provenance]) -> Result<()> {
    if provenance.is_empty() {
        return Err(ProjectError::MissingProvenance { change_index });
    }
    for entry in provenance {
        let invalid = |reason: &str| ProjectError::InvalidProvenance {
            change_index,
            reason: reason.to_owned(),
        };
        let require = |field: &str, value: &str| {
            if value.trim().is_empty() {
                Err(invalid(&format!("{field} must not be empty")))
            } else {
                Ok(())
            }
        };
        match &entry.source {
            crate::ProvenanceSource::ConversationMessage {
                conversation_id,
                message_id,
            } => {
                require("conversation_id", conversation_id)?;
                require("message_id", message_id)?;
            }
            crate::ProvenanceSource::Repository {
                repository,
                revision,
                evidence_hash,
                ..
            } => {
                require("repository", repository)?;
                require("repository revision", revision)?;
                require("evidence_hash", evidence_hash)?;
            }
            crate::ProvenanceSource::Experiment { trace_id, .. } => {
                require("trace_id", trace_id)?;
            }
            crate::ProvenanceSource::Research { uri, .. } => require("research uri", uri)?,
            crate::ProvenanceSource::DeliveryEvidence {
                delivery_id,
                evidence_id,
            } => {
                require("delivery_id", delivery_id)?;
                require("evidence_id", evidence_id)?;
            }
            crate::ProvenanceSource::Manual { reference } => {
                require("manual reference", reference)?;
            }
            crate::ProvenanceSource::Custom {
                source_type,
                reference,
            } => {
                require("custom source_type", source_type)?;
                require("custom reference", reference)?;
            }
        }
        if let Some(freshness) = &entry.freshness {
            if freshness
                .valid_until
                .is_some_and(|valid_until| valid_until < freshness.observed_at)
            {
                return Err(invalid("freshness expires before it was observed"));
            }
        }
    }
    Ok(())
}

fn current_node_mut(
    nodes: &mut BTreeMap<NodeId, PlanNode>,
    node_id: NodeId,
) -> Result<&mut PlanNode> {
    let node = nodes
        .get_mut(&node_id)
        .ok_or(ProjectError::NodeNotFound(node_id))?;
    if !node.is_current() {
        return Err(ProjectError::NodeAlreadySuperseded(node_id));
    }
    Ok(node)
}

fn validate_graph(
    nodes: &BTreeMap<NodeId, PlanNode>,
    edges: &BTreeMap<EdgeId, PlanEdge>,
) -> Result<()> {
    for node in nodes.values().filter(|node| node.is_current()) {
        validate_node_data(node.id, &node.kind, &node.data)?;
        validate_node_references(node, nodes)?;
    }
    for edge in edges.values().filter(|edge| edge.is_current()) {
        validate_edge(edge, nodes)?;
    }
    Ok(())
}

fn validate_node_data(node_id: NodeId, kind: &NodeKind, data: &NodeData) -> Result<()> {
    let compatible = matches!(
        (kind, data),
        (NodeKind::Decision, NodeData::Decision(_))
            | (NodeKind::Question, NodeData::Question(_))
            | (NodeKind::Assumption, NodeData::Assumption(_))
            | (NodeKind::Risk, NodeData::Risk(_))
            | (NodeKind::Milestone, NodeData::Milestone(_))
            | (NodeKind::Outcome, NodeData::Outcome(_))
            | (
                NodeKind::Intent
                    | NodeKind::Requirement
                    | NodeKind::Constraint
                    | NodeKind::NonGoal
                    | NodeKind::Option
                    | NodeKind::RepositoryObservation
                    | NodeKind::ResearchFinding
                    | NodeKind::Workstream
                    | NodeKind::AcceptanceBehavior
                    | NodeKind::ExecutionSlice
                    | NodeKind::Custom(_),
                NodeData::Generic { .. }
            )
    );
    if !compatible {
        return Err(ProjectError::IncompatibleNodeData {
            node_id,
            kind: kind.to_string(),
        });
    }

    match data {
        NodeData::Generic { status, .. } => validate_text("node status", status),
        NodeData::Decision(state) => {
            if matches!(
                state.status,
                crate::DecisionStatus::Accepted | crate::DecisionStatus::Delegated
            ) && state.decided_by.is_none()
            {
                return Err(ProjectError::InvalidNodeState {
                    node_id,
                    reason: "settled decision must identify who decided it".to_owned(),
                });
            }
            Ok(())
        }
        NodeData::Question(state) => {
            if state.status == crate::QuestionStatus::Resolved && state.resolution.is_none() {
                return Err(ProjectError::InvalidNodeState {
                    node_id,
                    reason: "resolved question must include a resolution".to_owned(),
                });
            }
            Ok(())
        }
        NodeData::Assumption(state) => validate_text("assumption impact", &state.impact),
        NodeData::Risk(_) | NodeData::Milestone(_) | NodeData::Outcome(_) => Ok(()),
    }
}

fn validate_node_references(node: &PlanNode, nodes: &BTreeMap<NodeId, PlanNode>) -> Result<()> {
    let require = |referenced_id: NodeId, field: &'static str, expected: Option<NodeKind>| {
        let referenced = nodes
            .get(&referenced_id)
            .filter(|referenced| referenced.is_current())
            .ok_or(ProjectError::MissingNodeReference {
                node_id: node.id,
                referenced_id,
                field,
            })?;
        if let Some(expected) = expected {
            if referenced.kind != expected {
                return Err(ProjectError::InvalidNodeState {
                    node_id: node.id,
                    reason: format!("{field} must reference a {expected} node"),
                });
            }
        }
        Ok(())
    };

    match &node.data {
        NodeData::Decision(state) => {
            if let Some(option) = state.selected_option {
                require(option, "selected_option", Some(NodeKind::Option))?;
            }
        }
        NodeData::Question(state) => {
            if let Some(scope) = state.blocking_scope {
                require(scope, "blocking_scope", None)?;
            }
            if let Some(milestone) = state.due_milestone {
                require(milestone, "due_milestone", Some(NodeKind::Milestone))?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn validate_edge_draft(
    edge: &crate::PlanEdgeDraft,
    nodes: &BTreeMap<NodeId, PlanNode>,
) -> Result<()> {
    validate_edge_parts(edge.id, edge.from, &edge.relation, edge.to, nodes)
}

fn validate_edge(edge: &PlanEdge, nodes: &BTreeMap<NodeId, PlanNode>) -> Result<()> {
    validate_edge_parts(edge.id, edge.from, &edge.relation, edge.to, nodes)
}

fn validate_edge_parts(
    edge_id: EdgeId,
    from: NodeId,
    relation: &EdgeRelation,
    to: NodeId,
    nodes: &BTreeMap<NodeId, PlanNode>,
) -> Result<()> {
    if from == to {
        return Err(ProjectError::SelfReferentialEdge {
            edge_id,
            node_id: from,
        });
    }
    let source = nodes
        .get(&from)
        .filter(|node| node.is_current())
        .ok_or(ProjectError::NodeNotFound(from))?;
    let target = nodes
        .get(&to)
        .filter(|node| node.is_current())
        .ok_or(ProjectError::NodeNotFound(to))?;

    let valid = match relation {
        EdgeRelation::Supports => {
            matches!(
                source.kind,
                NodeKind::Requirement
                    | NodeKind::RepositoryObservation
                    | NodeKind::ResearchFinding
                    | NodeKind::AcceptanceBehavior
                    | NodeKind::Decision
                    | NodeKind::Milestone
            ) && matches!(
                target.kind,
                NodeKind::Outcome
                    | NodeKind::Requirement
                    | NodeKind::Assumption
                    | NodeKind::Decision
                    | NodeKind::Milestone
                    | NodeKind::ExecutionSlice
            )
        }
        EdgeRelation::Challenges => {
            matches!(
                source.kind,
                NodeKind::RepositoryObservation | NodeKind::ResearchFinding | NodeKind::Risk
            ) && matches!(
                target.kind,
                NodeKind::Assumption
                    | NodeKind::Decision
                    | NodeKind::Requirement
                    | NodeKind::Outcome
            )
        }
        EdgeRelation::Resolves => {
            source.kind == NodeKind::Decision && target.kind == NodeKind::Question
        }
        EdgeRelation::Supersedes => source.kind == target.kind,
        EdgeRelation::Advances => {
            source.kind == NodeKind::Milestone && target.kind == NodeKind::Outcome
        }
        EdgeRelation::Realizes => {
            source.kind == NodeKind::ExecutionSlice && target.kind == NodeKind::Milestone
        }
        EdgeRelation::Verifies => {
            source.kind == NodeKind::AcceptanceBehavior
                && matches!(
                    target.kind,
                    NodeKind::ExecutionSlice
                        | NodeKind::Requirement
                        | NodeKind::Outcome
                        | NodeKind::Milestone
                )
        }
        EdgeRelation::Threatens => {
            source.kind == NodeKind::Risk
                && matches!(
                    target.kind,
                    NodeKind::Outcome | NodeKind::Milestone | NodeKind::ExecutionSlice
                )
        }
        EdgeRelation::DependsOn | EdgeRelation::RelatedTo | EdgeRelation::Custom(_) => true,
        EdgeRelation::PartOf => matches!(
            target.kind,
            NodeKind::Workstream | NodeKind::Milestone | NodeKind::ExecutionSlice
        ),
    };
    if !valid {
        return Err(ProjectError::InvalidRelationship {
            edge_id,
            reason: format!("{} {} {}", source.kind, relation, target.kind),
        });
    }
    Ok(())
}

fn validate_text(field: &'static str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(ProjectError::EmptyField { field });
    }
    Ok(())
}

fn validate_message_ref(message: &MessageRef) -> Result<()> {
    validate_text("conversation_id", &message.conversation_id)?;
    validate_text("message_id", &message.message_id)
}

fn canonical_revision_id(project: &Project, revision: &ProjectRevision) -> Result<RevisionId> {
    #[derive(Serialize)]
    struct RevisionContent<'a> {
        project: &'a Project,
        request_id: &'a str,
        parent_revision: &'a Option<RevisionId>,
        sequence: u64,
        author: &'a RevisionAuthor,
        summary: &'a str,
        source_message: &'a Option<MessageRef>,
        project_provenance: &'a [Provenance],
        created_at: DateTime<Utc>,
        nodes: &'a BTreeMap<NodeId, PlanNode>,
        edges: &'a BTreeMap<EdgeId, PlanEdge>,
    }

    let content = RevisionContent {
        project,
        request_id: &revision.request_id,
        parent_revision: &revision.parent_revision,
        sequence: revision.sequence,
        author: &revision.author,
        summary: &revision.summary,
        source_message: &revision.source_message,
        project_provenance: &revision.project_provenance,
        created_at: revision.created_at,
        nodes: &revision.nodes,
        edges: &revision.edges,
    };
    let value = serde_json::to_value(content)
        .map_err(|error| ProjectError::CanonicalSerialization(error.to_string()))?;
    let canonical = canonicalize_json(value);
    let bytes = serde_json::to_vec(&canonical)
        .map_err(|error| ProjectError::CanonicalSerialization(error.to_string()))?;
    Ok(RevisionId::from_hash(hex::encode(Sha256::digest(bytes))))
}

fn canonicalize_json(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted = map
                .into_iter()
                .map(|(key, value)| (key, canonicalize_json(value)))
                .collect();
            Value::Object(sorted)
        }
        Value::Array(values) => Value::Array(values.into_iter().map(canonicalize_json).collect()),
        other => other,
    }
}
