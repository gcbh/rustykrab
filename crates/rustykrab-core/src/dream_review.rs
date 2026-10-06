//! Frozen project reviews and meta-evaluation receipts. Model grades are judgments,
//! never ground truth or permission to execute a proposed change.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const REVIEW_ONLY: &str = "read_only_review";
pub const RUBRIC: &str = "dreaming-v1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReviewEvidence {
    pub id: String,
    pub source: String,
    /// A bounded, frozen observation; repository text is untrusted source material.
    pub content: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReviewInput {
    pub project_id: String,
    pub revision: String,
    pub project: Value,
    pub observations: Vec<ReviewEvidence>,
    pub omissions: Vec<String>,
    pub captured_at: DateTime<Utc>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Citation {
    pub evidence_id: String,
    pub quote: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DreamIdea {
    pub key: String,
    pub title: String,
    pub observed: String,
    pub change: String,
    pub metric: String,
    pub expected_movement: String,
    pub experiment: String,
    pub risk: String,
    pub rollback: String,
    pub citations: Vec<Citation>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GeneratedReview {
    pub ideas: Vec<DreamIdea>,
    pub abstention: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct IdeaAssessment {
    pub index: usize,
    /// 0 (unsupported) through 4 (strong). All scores are model judgments.
    pub evidence: u8,
    pub usefulness: u8,
    pub novelty: u8,
    pub testability: u8,
    pub recommend: bool,
    pub reason: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CalibrationAssessment {
    pub case: String,
    pub evidence: u8,
    pub novelty: u8,
    pub testability: u8,
    pub recommend: bool,
    pub reason: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MetaReview {
    pub rubric: String,
    /// Known negative controls check basic reviewer calibration, not real utility.
    pub calibration: Vec<CalibrationAssessment>,
    pub assessments: Vec<IdeaAssessment>,
    pub coverage: String,
    pub blind_spots: Vec<String>,
    pub improvements: Vec<String>,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStage {
    Preparing,
    Generating,
    Evaluating,
    Publishing,
    Completed,
    Failed,
}
impl ReviewStage {
    pub fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectReview {
    pub id: String,
    pub version: u64,
    pub input: ReviewInput,
    pub stage: ReviewStage,
    pub generator_item: Option<String>,
    pub evaluator_item: Option<String>,
    pub generated: Option<GeneratedReview>,
    pub meta: Option<MetaReview>,
    pub filed: Vec<String>,
    pub skipped: Vec<String>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DreamMetaMetrics {
    pub reviews: usize,
    pub completed: usize,
    pub failed: usize,
    pub pending: usize,
    pub calibrated_reviews: usize,
    pub eligible_projects: usize,
    pub reviewed_projects: usize,
    pub proposal_count: usize,
    pub accepted: usize,
    pub declined: usize,
    pub pending_decisions: usize,
    pub measured_outcomes: usize,
    pub improved_outcomes: usize,
    pub unmeasurable_outcomes: usize,
    /// None when there is no decision/measurement, rather than a fabricated zero.
    pub acceptance_rate: Option<f64>,
    pub improvement_rate: Option<f64>,
    pub tokens: u64,
    pub wall_seconds: u64,
    pub warnings: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReviewSummary {
    pub id: String,
    pub project_id: String,
    pub revision: String,
    pub stage: ReviewStage,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub generator_item: Option<String>,
    pub evaluator_item: Option<String>,
    pub generated: Option<GeneratedReview>,
    pub meta: Option<MetaReview>,
    pub filed: Vec<String>,
    pub skipped: Vec<String>,
    pub error: Option<String>,
}
impl From<ProjectReview> for ReviewSummary {
    fn from(r: ProjectReview) -> Self {
        Self {
            id: r.id,
            project_id: r.input.project_id,
            revision: r.input.revision,
            stage: r.stage,
            created_at: r.created_at,
            updated_at: r.updated_at,
            generator_item: r.generator_item,
            evaluator_item: r.evaluator_item,
            generated: r.generated,
            meta: r.meta,
            filed: r.filed,
            skipped: r.skipped,
            error: r.error,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DreamingView {
    pub enabled: bool,
    pub interval_seconds: u64,
    pub project_ids: Vec<String>,
    pub last_evaluation: Option<DateTime<Utc>>,
    pub last_analysis: Option<DateTime<Utc>>,
    pub outcome_records: u32,
    pub rubric: String,
    pub metrics: DreamMetaMetrics,
    pub reviews: Vec<ReviewSummary>,
}
