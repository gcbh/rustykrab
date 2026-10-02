use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{EdgeId, NodeId, ProjectId, Provenance};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub repository_id: Option<String>,
    pub title: String,
    pub status: ProjectStatus,
    pub judgment_policy: JudgmentPolicy,
    pub canonical_conversation_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectStatus {
    Active,
    Paused,
    Completed,
    Archived,
    Custom(String),
}

/// Durable natural-language delegation boundaries. Enforcement layers may
/// narrow these rules, but must never infer authority absent from them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JudgmentPolicy {
    pub statement: String,
    pub delegated_scopes: Vec<String>,
    pub reserved_decisions: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Intent,
    Outcome,
    Requirement,
    Constraint,
    NonGoal,
    Decision,
    Option,
    Assumption,
    Question,
    Risk,
    RepositoryObservation,
    ResearchFinding,
    Workstream,
    Milestone,
    AcceptanceBehavior,
    ExecutionSlice,
    Custom(String),
}

impl NodeKind {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Intent => "intent",
            Self::Outcome => "outcome",
            Self::Requirement => "requirement",
            Self::Constraint => "constraint",
            Self::NonGoal => "non_goal",
            Self::Decision => "decision",
            Self::Option => "option",
            Self::Assumption => "assumption",
            Self::Question => "question",
            Self::Risk => "risk",
            Self::RepositoryObservation => "repository_observation",
            Self::ResearchFinding => "research_finding",
            Self::Workstream => "workstream",
            Self::Milestone => "milestone",
            Self::AcceptanceBehavior => "acceptance_behavior",
            Self::ExecutionSlice => "execution_slice",
            Self::Custom(value) => value,
        }
    }
}

impl std::fmt::Display for NodeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "state", rename_all = "snake_case")]
pub enum NodeData {
    Generic {
        status: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        attributes: BTreeMap<String, Value>,
    },
    Decision(DecisionState),
    Question(QuestionState),
    Assumption(AssumptionState),
    Risk(RiskState),
    Milestone(MilestoneState),
    Outcome(OutcomeState),
}

impl NodeData {
    pub fn generic(status: impl Into<String>) -> Self {
        Self::Generic {
            status: status.into(),
            attributes: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionState {
    pub status: DecisionStatus,
    pub selected_option: Option<NodeId>,
    pub rationale: Option<String>,
    pub authority_basis: Option<String>,
    pub reversible: bool,
    pub decided_by: Option<DecisionMaker>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionStatus {
    Proposed,
    Accepted,
    Delegated,
    Rejected,
    Deferred,
    Superseded,
    Custom(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionMaker {
    User,
    Agent,
    System,
    Custom(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionState {
    pub status: QuestionStatus,
    pub impact: QuestionImpact,
    pub decision_owner: DecisionOwner,
    pub blocking_scope: Option<NodeId>,
    pub default_action: Option<String>,
    pub due_milestone: Option<NodeId>,
    pub resolution: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionStatus {
    Open,
    Resolved,
    Deferred,
    Delegated,
    Obsolete,
    Custom(String),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionImpact {
    BlockingNow,
    BlockingLater,
    Researchable,
    Defaultable,
    Delegated,
    Obsolete,
    Custom(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionOwner {
    User,
    Agent,
    Shared,
    External(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssumptionState {
    pub status: AssumptionStatus,
    pub impact: String,
    pub validation_method: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssumptionStatus {
    Unvalidated,
    Validated,
    Challenged,
    Invalidated,
    Obsolete,
    Custom(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RiskState {
    pub status: RiskStatus,
    pub likelihood: RiskLevel,
    pub impact: RiskLevel,
    pub mitigation: Option<String>,
    pub trigger: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskStatus {
    Open,
    Mitigated,
    Accepted,
    Realized,
    Closed,
    Custom(String),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Low,
    Medium,
    High,
    Critical,
    Custom(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MilestoneState {
    pub status: MilestoneStatus,
    pub exit_conditions: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MilestoneStatus {
    Proposed,
    Planned,
    InProgress,
    Achieved,
    Deferred,
    Cancelled,
    Custom(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutcomeState {
    pub status: OutcomeStatus,
    pub success_measures: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeStatus {
    Desired,
    InProgress,
    Achieved,
    Challenged,
    Abandoned,
    Custom(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanNodeDraft {
    pub id: NodeId,
    pub kind: NodeKind,
    pub title: String,
    pub body: String,
    pub data: NodeData,
    pub provenance: Vec<Provenance>,
}

impl PlanNodeDraft {
    pub fn new(
        id: NodeId,
        kind: NodeKind,
        title: impl Into<String>,
        body: impl Into<String>,
        data: NodeData,
        provenance: Vec<Provenance>,
    ) -> Self {
        Self {
            id,
            kind,
            title: title.into(),
            body: body.into(),
            data,
            provenance,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanNode {
    pub id: NodeId,
    pub kind: NodeKind,
    pub title: String,
    pub body: String,
    pub data: NodeData,
    pub provenance: Vec<Provenance>,
    pub introduced_revision: u64,
    pub updated_revision: u64,
    pub superseded_revision: Option<u64>,
    pub superseded_by: Option<NodeId>,
}

impl PlanNode {
    pub fn is_current(&self) -> bool {
        self.superseded_revision.is_none()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeRelation {
    Supports,
    Challenges,
    Resolves,
    Supersedes,
    Advances,
    Realizes,
    Verifies,
    Threatens,
    DependsOn,
    PartOf,
    RelatedTo,
    Custom(String),
}

impl EdgeRelation {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Supports => "supports",
            Self::Challenges => "challenges",
            Self::Resolves => "resolves",
            Self::Supersedes => "supersedes",
            Self::Advances => "advances",
            Self::Realizes => "realizes",
            Self::Verifies => "verifies",
            Self::Threatens => "threatens",
            Self::DependsOn => "depends_on",
            Self::PartOf => "part_of",
            Self::RelatedTo => "related_to",
            Self::Custom(value) => value,
        }
    }
}

impl std::fmt::Display for EdgeRelation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanEdgeDraft {
    pub id: EdgeId,
    pub from: NodeId,
    pub relation: EdgeRelation,
    pub to: NodeId,
    pub provenance: Vec<Provenance>,
}

impl PlanEdgeDraft {
    pub fn new(
        id: EdgeId,
        from: NodeId,
        relation: EdgeRelation,
        to: NodeId,
        provenance: Vec<Provenance>,
    ) -> Self {
        Self {
            id,
            from,
            relation,
            to,
            provenance,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanEdge {
    pub id: EdgeId,
    pub from: NodeId,
    pub relation: EdgeRelation,
    pub to: NodeId,
    pub provenance: Vec<Provenance>,
    pub introduced_revision: u64,
    pub superseded_revision: Option<u64>,
}

impl PlanEdge {
    pub fn is_current(&self) -> bool {
        self.superseded_revision.is_none()
    }
}
