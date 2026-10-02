//! Pure domain rules for durable, conversational project planning.
//!
//! This crate deliberately contains no persistence, model, network, or process
//! integration. It accepts explicit inputs and produces immutable revisions
//! and deterministic projections that application layers can store or expose.

mod change_set;
mod error;
mod ids;
mod model;
mod projection;
mod provenance;
mod revision;

pub use change_set::{PlanChange, PlanChangeSet, PlanNodePatch, ProjectPatch};
pub use error::{ProjectError, Result};
pub use ids::{EdgeId, NodeId, ProjectId, RevisionId, RevisionIdParseError};
pub use model::{
    AssumptionState, AssumptionStatus, DecisionMaker, DecisionOwner, DecisionState, DecisionStatus,
    EdgeRelation, JudgmentPolicy, MilestoneState, MilestoneStatus, NodeData, NodeKind,
    OutcomeState, OutcomeStatus, PlanEdge, PlanEdgeDraft, PlanNode, PlanNodeDraft, Project,
    ProjectStatus, QuestionImpact, QuestionState, QuestionStatus, RiskLevel, RiskState, RiskStatus,
};
pub use projection::{
    ArchitectureProjection, BehaviorCatalogProjection, BriefProjection,
    CurrentUnderstandingProjection, DecisionEntry, DecisionLogProjection, EdgeSummary,
    MilestoneEntry, NodeSummary, ProjectProjections, Projection, ProjectionKind, QuestionEntry,
    QuestionProjection, RevisionComparison, RiskEntry, RiskProjection, RoadmapProjection,
};
pub use provenance::{
    Confidence, Freshness, MessageRef, Provenance, ProvenanceClassification, ProvenanceSource,
};
pub use revision::{CreateProject, ProjectRevision, ProjectSnapshot, RevisionAuthor};
