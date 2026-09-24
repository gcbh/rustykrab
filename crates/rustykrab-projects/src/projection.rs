use serde::{Deserialize, Serialize};

use crate::{
    DecisionState, EdgeId, EdgeRelation, MilestoneState, NodeData, NodeId, NodeKind, PlanNode,
    ProjectError, ProjectRevision, QuestionState, Result, RevisionId, RiskState,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionKind {
    Brief,
    Roadmap,
    Decisions,
    Questions,
    Risks,
    Architecture,
    BehaviorCatalog,
    CurrentUnderstanding,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "projection", rename_all = "snake_case")]
pub enum Projection {
    Brief(BriefProjection),
    Roadmap(RoadmapProjection),
    Decisions(DecisionLogProjection),
    Questions(QuestionProjection),
    Risks(RiskProjection),
    Architecture(ArchitectureProjection),
    BehaviorCatalog(BehaviorCatalogProjection),
    CurrentUnderstanding(CurrentUnderstandingProjection),
}

impl Projection {
    pub fn revision_id(&self) -> &RevisionId {
        match self {
            Self::Brief(value) => &value.revision_id,
            Self::Roadmap(value) => &value.revision_id,
            Self::Decisions(value) => &value.revision_id,
            Self::Questions(value) => &value.revision_id,
            Self::Risks(value) => &value.revision_id,
            Self::Architecture(value) => &value.revision_id,
            Self::BehaviorCatalog(value) => &value.revision_id,
            Self::CurrentUnderstanding(value) => &value.revision_id,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectProjections {
    pub revision_id: RevisionId,
    pub brief: BriefProjection,
    pub roadmap: RoadmapProjection,
    pub decisions: DecisionLogProjection,
    pub questions: QuestionProjection,
    pub risks: RiskProjection,
    pub architecture: ArchitectureProjection,
    pub behavior_catalog: BehaviorCatalogProjection,
    pub current_understanding: CurrentUnderstandingProjection,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeSummary {
    pub id: NodeId,
    pub kind: NodeKind,
    pub title: String,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeSummary {
    pub id: EdgeId,
    pub from: NodeId,
    pub relation: EdgeRelation,
    pub to: NodeId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BriefProjection {
    pub revision_id: RevisionId,
    pub intents: Vec<NodeSummary>,
    pub outcomes: Vec<NodeSummary>,
    pub requirements: Vec<NodeSummary>,
    pub constraints: Vec<NodeSummary>,
    pub non_goals: Vec<NodeSummary>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoadmapProjection {
    pub revision_id: RevisionId,
    pub workstreams: Vec<NodeSummary>,
    pub milestones: Vec<MilestoneEntry>,
    pub dependencies: Vec<EdgeSummary>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MilestoneEntry {
    pub node: NodeSummary,
    pub state: MilestoneState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionLogProjection {
    pub revision_id: RevisionId,
    /// Includes superseded decisions so the view is an audit log, not merely a
    /// list of the currently selected choices.
    pub decisions: Vec<DecisionEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionEntry {
    pub node: NodeSummary,
    pub state: DecisionState,
    pub current: bool,
    pub introduced_revision: u64,
    pub superseded_revision: Option<u64>,
    pub superseded_by: Option<NodeId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionProjection {
    pub revision_id: RevisionId,
    pub questions: Vec<QuestionEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionEntry {
    pub node: NodeSummary,
    pub state: QuestionState,
    pub current: bool,
    pub introduced_revision: u64,
    pub superseded_revision: Option<u64>,
    pub superseded_by: Option<NodeId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RiskProjection {
    pub revision_id: RevisionId,
    pub risks: Vec<RiskEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RiskEntry {
    pub node: NodeSummary,
    pub state: RiskState,
}

/// Source-backed inputs useful for generating a narrative architecture view.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchitectureProjection {
    pub revision_id: RevisionId,
    pub decisions: Vec<NodeSummary>,
    pub constraints: Vec<NodeSummary>,
    pub repository_observations: Vec<NodeSummary>,
    pub relationships: Vec<EdgeSummary>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BehaviorCatalogProjection {
    pub revision_id: RevisionId,
    pub behaviors: Vec<NodeSummary>,
    pub verification_relationships: Vec<EdgeSummary>,
}

/// Complete current graph material for deterministic context reconstruction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentUnderstandingProjection {
    pub revision_id: RevisionId,
    pub nodes: Vec<NodeSummary>,
    pub relationships: Vec<EdgeSummary>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevisionComparison {
    pub from_revision: RevisionId,
    pub to_revision: RevisionId,
    pub added_nodes: Vec<NodeSummary>,
    pub updated_nodes: Vec<NodeSummary>,
    pub superseded_nodes: Vec<NodeSummary>,
    pub added_edges: Vec<EdgeSummary>,
    pub retired_edges: Vec<EdgeSummary>,
}

impl ProjectRevision {
    pub fn projections(&self) -> ProjectProjections {
        ProjectProjections {
            revision_id: self.id.clone(),
            brief: self.brief_projection(),
            roadmap: self.roadmap_projection(),
            decisions: self.decision_log_projection(),
            questions: self.question_projection(),
            risks: self.risk_projection(),
            architecture: self.architecture_projection(),
            behavior_catalog: self.behavior_catalog_projection(),
            current_understanding: self.current_understanding_projection(),
        }
    }

    pub fn projection(&self, kind: ProjectionKind) -> Projection {
        match kind {
            ProjectionKind::Brief => Projection::Brief(self.brief_projection()),
            ProjectionKind::Roadmap => Projection::Roadmap(self.roadmap_projection()),
            ProjectionKind::Decisions => Projection::Decisions(self.decision_log_projection()),
            ProjectionKind::Questions => Projection::Questions(self.question_projection()),
            ProjectionKind::Risks => Projection::Risks(self.risk_projection()),
            ProjectionKind::Architecture => {
                Projection::Architecture(self.architecture_projection())
            }
            ProjectionKind::BehaviorCatalog => {
                Projection::BehaviorCatalog(self.behavior_catalog_projection())
            }
            ProjectionKind::CurrentUnderstanding => {
                Projection::CurrentUnderstanding(self.current_understanding_projection())
            }
        }
    }

    pub fn brief_projection(&self) -> BriefProjection {
        BriefProjection {
            revision_id: self.id.clone(),
            intents: summaries(self, NodeKind::Intent),
            outcomes: summaries(self, NodeKind::Outcome),
            requirements: summaries(self, NodeKind::Requirement),
            constraints: summaries(self, NodeKind::Constraint),
            non_goals: summaries(self, NodeKind::NonGoal),
        }
    }

    pub fn roadmap_projection(&self) -> RoadmapProjection {
        let mut milestones: Vec<_> = self
            .current_nodes()
            .filter_map(|node| match &node.data {
                NodeData::Milestone(state) => Some(MilestoneEntry {
                    node: node.into(),
                    state: state.clone(),
                }),
                _ => None,
            })
            .collect();
        milestones.sort_by(|left, right| summary_order(&left.node, &right.node));

        let mut dependencies: Vec<_> = self
            .current_edges()
            .filter(|edge| edge.relation == EdgeRelation::DependsOn)
            .map(|edge| EdgeSummary {
                id: edge.id,
                from: edge.from,
                relation: edge.relation.clone(),
                to: edge.to,
            })
            .collect();
        dependencies.sort_by_key(|edge| edge.id);

        RoadmapProjection {
            revision_id: self.id.clone(),
            workstreams: summaries(self, NodeKind::Workstream),
            milestones,
            dependencies,
        }
    }

    pub fn decision_log_projection(&self) -> DecisionLogProjection {
        let mut decisions: Vec<_> = self
            .nodes
            .values()
            .filter_map(|node| match &node.data {
                NodeData::Decision(state) => Some(DecisionEntry {
                    node: node.into(),
                    state: state.clone(),
                    current: node.is_current(),
                    introduced_revision: node.introduced_revision,
                    superseded_revision: node.superseded_revision,
                    superseded_by: node.superseded_by,
                }),
                _ => None,
            })
            .collect();
        decisions.sort_by(|left, right| {
            left.introduced_revision
                .cmp(&right.introduced_revision)
                .then_with(|| summary_order(&left.node, &right.node))
        });
        DecisionLogProjection {
            revision_id: self.id.clone(),
            decisions,
        }
    }

    pub fn question_projection(&self) -> QuestionProjection {
        let mut questions: Vec<_> = self
            .nodes
            .values()
            .filter_map(|node| match &node.data {
                NodeData::Question(state) => Some(QuestionEntry {
                    node: node.into(),
                    state: state.clone(),
                    current: node.is_current(),
                    introduced_revision: node.introduced_revision,
                    superseded_revision: node.superseded_revision,
                    superseded_by: node.superseded_by,
                }),
                _ => None,
            })
            .collect();
        questions.sort_by(|left, right| {
            question_rank(&left.state)
                .cmp(&question_rank(&right.state))
                .then_with(|| summary_order(&left.node, &right.node))
        });
        QuestionProjection {
            revision_id: self.id.clone(),
            questions,
        }
    }

    pub fn risk_projection(&self) -> RiskProjection {
        let mut risks: Vec<_> = self
            .current_nodes()
            .filter_map(|node| match &node.data {
                NodeData::Risk(state) => Some(RiskEntry {
                    node: node.into(),
                    state: state.clone(),
                }),
                _ => None,
            })
            .collect();
        risks.sort_by(|left, right| summary_order(&left.node, &right.node));
        RiskProjection {
            revision_id: self.id.clone(),
            risks,
        }
    }

    pub fn architecture_projection(&self) -> ArchitectureProjection {
        ArchitectureProjection {
            revision_id: self.id.clone(),
            decisions: summaries(self, NodeKind::Decision),
            constraints: summaries(self, NodeKind::Constraint),
            repository_observations: summaries(self, NodeKind::RepositoryObservation),
            relationships: current_edge_summaries(self, |relation| {
                matches!(
                    relation,
                    EdgeRelation::Supports
                        | EdgeRelation::Challenges
                        | EdgeRelation::DependsOn
                        | EdgeRelation::PartOf
                        | EdgeRelation::RelatedTo
                        | EdgeRelation::Custom(_)
                )
            }),
        }
    }

    pub fn behavior_catalog_projection(&self) -> BehaviorCatalogProjection {
        BehaviorCatalogProjection {
            revision_id: self.id.clone(),
            behaviors: summaries(self, NodeKind::AcceptanceBehavior),
            verification_relationships: current_edge_summaries(self, |relation| {
                *relation == EdgeRelation::Verifies
            }),
        }
    }

    pub fn current_understanding_projection(&self) -> CurrentUnderstandingProjection {
        let mut nodes: Vec<_> = self.current_nodes().map(Into::into).collect();
        nodes.sort_by(summary_order);
        CurrentUnderstandingProjection {
            revision_id: self.id.clone(),
            nodes,
            relationships: current_edge_summaries(self, |_| true),
        }
    }

    pub fn compare(&self, newer: &ProjectRevision) -> Result<RevisionComparison> {
        if self.project_id != newer.project_id {
            return Err(ProjectError::ProjectMismatch {
                expected: self.project_id.to_string(),
                actual: newer.project_id.to_string(),
            });
        }
        if self.sequence > newer.sequence {
            return Err(ProjectError::InvalidRevisionComparison {
                from_sequence: self.sequence,
                to_sequence: newer.sequence,
            });
        }

        let mut added_nodes = Vec::new();
        let mut updated_nodes = Vec::new();
        let mut superseded_nodes = Vec::new();
        for (id, node) in &newer.nodes {
            match self.nodes.get(id) {
                None => {
                    added_nodes.push(node.into());
                    if !node.is_current() {
                        superseded_nodes.push(node.into());
                    }
                }
                Some(old) if old.is_current() && !node.is_current() => {
                    superseded_nodes.push(node.into());
                }
                Some(old) if old != node && node.is_current() => updated_nodes.push(node.into()),
                _ => {}
            }
        }

        let mut added_edges: Vec<EdgeSummary> = Vec::new();
        let mut retired_edges: Vec<EdgeSummary> = Vec::new();
        for (id, edge) in &newer.edges {
            match self.edges.get(id) {
                None => {
                    added_edges.push(edge.into());
                    if !edge.is_current() {
                        retired_edges.push(edge.into());
                    }
                }
                Some(old) if old.is_current() && !edge.is_current() => {
                    retired_edges.push(edge.into());
                }
                _ => {}
            }
        }
        added_nodes.sort_by(summary_order);
        updated_nodes.sort_by(summary_order);
        superseded_nodes.sort_by(summary_order);
        added_edges.sort_by_key(|edge| edge.id);
        retired_edges.sort_by_key(|edge| edge.id);

        Ok(RevisionComparison {
            from_revision: self.id.clone(),
            to_revision: newer.id.clone(),
            added_nodes,
            updated_nodes,
            superseded_nodes,
            added_edges,
            retired_edges,
        })
    }
}

impl From<&PlanNode> for NodeSummary {
    fn from(node: &PlanNode) -> Self {
        Self {
            id: node.id,
            kind: node.kind.clone(),
            title: node.title.clone(),
            body: node.body.clone(),
        }
    }
}

impl From<&crate::PlanEdge> for EdgeSummary {
    fn from(edge: &crate::PlanEdge) -> Self {
        Self {
            id: edge.id,
            from: edge.from,
            relation: edge.relation.clone(),
            to: edge.to,
        }
    }
}

fn summaries(revision: &ProjectRevision, kind: NodeKind) -> Vec<NodeSummary> {
    let mut values: Vec<NodeSummary> = revision
        .current_nodes()
        .filter(|node| node.kind == kind)
        .map(Into::into)
        .collect();
    values.sort_by(summary_order);
    values
}

fn summary_order(left: &NodeSummary, right: &NodeSummary) -> std::cmp::Ordering {
    left.title
        .to_lowercase()
        .cmp(&right.title.to_lowercase())
        .then_with(|| left.id.cmp(&right.id))
}

fn current_edge_summaries(
    revision: &ProjectRevision,
    include: impl Fn(&EdgeRelation) -> bool,
) -> Vec<EdgeSummary> {
    let mut values: Vec<EdgeSummary> = revision
        .current_edges()
        .filter(|edge| include(&edge.relation))
        .map(Into::into)
        .collect();
    values.sort_by_key(|edge| edge.id);
    values
}

fn question_rank(state: &QuestionState) -> u8 {
    use crate::QuestionImpact;
    match &state.impact {
        QuestionImpact::BlockingNow => 0,
        QuestionImpact::Researchable => 1,
        QuestionImpact::Defaultable => 2,
        QuestionImpact::Delegated => 3,
        QuestionImpact::BlockingLater => 4,
        QuestionImpact::Obsolete => 5,
        QuestionImpact::Custom(_) => 6,
    }
}
