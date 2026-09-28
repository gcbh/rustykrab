//! Evaluation as a dreaming pass (plan sections 1.1 and 10, Phase 6).
//!
//! One pass, deterministic and without a model:
//!
//! 1. Read the window's work records (live and archived items, their
//!    events and edges, their review facets), the questions the router
//!    delivered, the routing record, and the proposal records.
//! 2. Compute every expectation metric of section 1.1 from the stored
//!    events ([`metrics::compute`]), compare it with the previous pass
//!    ([`metrics::regressions`]) and store it.
//! 3. Bring proposal records up to date with their items and run
//!    probation on accepted ones ([`file::reconcile`]): a decision taken on
//!    the review surface is recorded with the named metric's baseline, and
//!    a change that landed a probation window ago is judged against it
//!    (moved, not moved, or regressed, which proposes a rollback).
//! 4. Look where section 10 says to look ([`criteria`]) and file what it
//!    finds ([`file::file_findings`]): only from ground-truth evidence
//!    (`SignalClass::is_ground_truth`, with the `internal` item for an
//!    unknown error the one exception), one live proposal per subject,
//!    a daily cap, and each proposal at the review tier its subject
//!    requires.
//!
//! Everything the pass touches is a trait here, so the pass runs against
//! in-memory fakes in tests. `store.rs` backs the store-shaped ones;
//! [`QuestionReader`] and [`RoutingRecordReader`] read Phases 4 and 3,
//! which the composition root wires to their tables ([`StaticQuestions`]
//! and [`StaticRouting`] are the in-memory implementations meanwhile);
//! [`ProposalFiler`] is the controller's validator, adapted in the CLI.

pub mod criteria;
pub mod facts;
pub mod file;
pub mod metrics;
pub mod store;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};

use rustykrab_core::outcome::AttributionKind;
use rustykrab_core::proposal::{
    EvaluationReport, MetricValue, ProposalOutcome, ReviewState, SkippedFinding,
};
use rustykrab_core::work::{ArtifactRef, PlanOutcome, WorkError, WorkItemDraft};
use rustykrab_core::Result;
use rustykrab_store::{ProposalEvidence, ProposalRow};

use crate::report::OutcomeSource;

pub use criteria::{Files, Finding, Thresholds};
pub use facts::{Facts, ItemRecord, RoutingEntry, SurfacedQuestion, WorkRecords};
pub use metrics::{Regression, RegressionRule};

/// The work records of a window.
#[async_trait]
pub trait WorkRecordSource: Send + Sync {
    /// Items created, closed or still open at or after `since` (live and
    /// archived), every event at or after it, the edges those items hold,
    /// and their review facets.
    async fn records(&self, since: DateTime<Utc>) -> Result<WorkRecords>;
}

/// Phase 4's `questions`, as the avoidable-escalation criterion reads
/// them.
#[async_trait]
pub trait QuestionReader: Send + Sync {
    /// Questions asked at or after `since`, answered or not.
    async fn questions(&self, since: DateTime<Utc>) -> Result<Vec<SurfacedQuestion>>;
}

/// Phase 3's routing record, per worker and class of work.
#[async_trait]
pub trait RoutingRecordReader: Send + Sync {
    async fn entries(&self) -> Result<Vec<RoutingEntry>>;
}

/// Questions held in memory: the tests' double, and the daemon's reader
/// until the `questions` table is wired (then it reads none).
#[derive(Debug, Clone, Default)]
pub struct StaticQuestions(pub Vec<SurfacedQuestion>);

#[async_trait]
impl QuestionReader for StaticQuestions {
    async fn questions(&self, since: DateTime<Utc>) -> Result<Vec<SurfacedQuestion>> {
        Ok(self
            .0
            .iter()
            .filter(|q| q.answered_at.is_none_or(|t| t >= since))
            .cloned()
            .collect())
    }
}

/// A routing record held in memory, as [`StaticQuestions`].
#[derive(Debug, Clone, Default)]
pub struct StaticRouting(pub Vec<RoutingEntry>);

#[async_trait]
impl RoutingRecordReader for StaticRouting {
    async fn entries(&self) -> Result<Vec<RoutingEntry>> {
        Ok(self.0.clone())
    }
}

/// Where the pass files: the controller's validator, which may still
/// refuse (the scope limit, a malformed draft).
#[async_trait]
pub trait ProposalFiler: Send + Sync {
    /// File one draft as dreaming.
    async fn file(&self, draft: WorkItemDraft) -> Result<PlanOutcome>;
    /// File the `internal` item for `error` in the controller's own shape,
    /// with `evidence` attached.
    async fn file_internal(
        &self,
        error: &WorkError,
        evidence: Vec<ArtifactRef>,
    ) -> Result<PlanOutcome>;
}

/// What the pass keeps: the metrics of each pass and the proposal records.
#[async_trait]
pub trait EvaluationLedger: Send + Sync {
    /// The previous pass's metrics; empty before the first.
    async fn latest_metrics(&self) -> Result<Vec<MetricValue>>;
    async fn record_metrics(&self, pass: &str, values: &[MetricValue]) -> Result<()>;
    async fn proposals(&self) -> Result<Vec<ProposalRow>>;
    async fn record_proposal(&self, row: &ProposalRow, evidence: &[ProposalEvidence])
        -> Result<()>;
    async fn record_review(
        &self,
        item: &str,
        review: ReviewState,
        decided_by: &str,
        decided_at: DateTime<Utc>,
        code_item: Option<&str>,
        baseline: Option<f64>,
    ) -> Result<()>;
    async fn record_outcome(
        &self,
        item: &str,
        outcome: ProposalOutcome,
        observed: Option<f64>,
        at: DateTime<Utc>,
    ) -> Result<()>;
}

/// The pass's knobs. Placeholders until nightly runs measure them, as the
/// graph caps are.
#[derive(Debug, Clone)]
pub struct EvaluationConfig {
    /// The trailing window every metric covers.
    pub window_days: u32,
    /// Proposals dreaming files in any 24 hours (section 10's rate limit).
    pub proposals_per_day: u32,
    /// How long a declined subject stays closed to a new proposal.
    pub decline_cooldown: Duration,
    /// How long an accepted change runs before its metric is judged.
    pub probation: Duration,
    pub thresholds: Thresholds,
    pub regression: RegressionRule,
    /// The actor dreaming files as, and the `filed_by` of its records.
    pub actor: String,
}

impl Default for EvaluationConfig {
    fn default() -> Self {
        EvaluationConfig {
            window_days: 7,
            proposals_per_day: 10,
            decline_cooldown: Duration::days(30),
            probation: Duration::days(7),
            thresholds: Thresholds::default(),
            regression: RegressionRule::default(),
            actor: "dreaming".to_string(),
        }
    }
}

/// One evaluation over its sources.
pub struct Evaluation {
    pub work: Arc<dyn WorkRecordSource>,
    pub outcomes: Arc<dyn OutcomeSource>,
    pub questions: Arc<dyn QuestionReader>,
    pub routing: Arc<dyn RoutingRecordReader>,
    pub filer: Arc<dyn ProposalFiler>,
    pub ledger: Arc<dyn EvaluationLedger>,
    pub config: EvaluationConfig,
}

impl Evaluation {
    /// Run one pass at `now`. The review surface's projection is the
    /// caller's to add to the report: it is not dreaming's.
    pub async fn run(&self, now: DateTime<Utc>) -> Result<EvaluationReport> {
        let cfg = &self.config;
        let since = now - Duration::days(i64::from(cfg.window_days));
        let records = self.work.records(since).await?;
        let facts = Facts::derive(&records);
        let mut skipped: Vec<SkippedFinding> = Vec::new();
        // Phases 3 and 4 may not be wired; a reader that fails reads as
        // none, and the report says so.
        let questions = match self.questions.questions(since).await {
            Ok(q) => q,
            Err(e) => {
                tracing::warn!(error = %e, "evaluation: questions unreadable");
                Vec::new()
            }
        };
        let routing = match self.routing.entries().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "evaluation: routing record unreadable");
                Vec::new()
            }
        };
        let mut rows = self.ledger.proposals().await?;

        let current = metrics::compute(&metrics::MetricInputs {
            facts: &facts,
            questions: &questions,
            routing: &routing,
            proposals: &rows,
            since,
            now,
            window_days: cfg.window_days,
        });
        let previous = self.ledger.latest_metrics().await?;
        let regressions = metrics::regressions(&previous, &current, &facts, cfg.regression);
        let pass = uuid::Uuid::new_v4().to_string();
        self.ledger.record_metrics(&pass, &current).await?;

        // Skills, on ground truth only: the gate starts at the read.
        let ground_truth = self.outcomes.tallies(AttributionKind::Skill, true).await?;
        let skill_rates: BTreeMap<String, f64> = ground_truth
            .iter()
            .filter_map(|(s, t)| t.success_rate(1).map(|r| (s.clone(), r)))
            .collect();
        let cite = self
            .outcomes
            .cite(AttributionKind::Skill, true, 20)
            .await
            .unwrap_or_default();
        // What proxy evidence alone would have flagged is reported, never
        // filed: that is the gate, made visible.
        let everything = self.outcomes.tallies(AttributionKind::Skill, false).await?;
        for (skill, tally) in &everything {
            let flagged_by_proxy = tally
                .success_rate(cfg.thresholds.min_sample)
                .is_some_and(|r| r <= cfg.thresholds.skill_rate_at_most);
            let on_ground_truth = ground_truth
                .iter()
                .any(|(s, t)| s == skill && t.success_rate(cfg.thresholds.min_sample).is_some());
            if flagged_by_proxy && !on_ground_truth {
                skipped.push(SkippedFinding {
                    criterion: rustykrab_core::proposal::Criterion::SkillOutcome,
                    subject: format!("skill:{skill}"),
                    why: "implicit or judge evidence only: the ground-truth gate files nothing \
                          from it"
                        .to_string(),
                });
            }
        }

        let (probation, regressed) = file::reconcile(
            &facts,
            &mut rows,
            &current,
            &skill_rates,
            self.ledger.as_ref(),
            cfg,
            now,
        )
        .await;

        let t = cfg.thresholds;
        let mut findings: Vec<Finding> = Vec::new();
        findings.extend(criteria::expectation_regressions(&regressions));
        findings.extend(regressed.iter().map(file::rollback_finding));
        findings.extend(criteria::avoidable_escalations(&questions));
        findings.extend(criteria::unknown_errors(&facts));
        findings.extend(criteria::recurring_fingerprints(&facts, t));
        findings.extend(criteria::capability_gaps(&facts, t));
        findings.extend(criteria::wasted_rungs(&facts, t));
        findings.extend(criteria::verification_misses(&facts, t));
        findings.extend(criteria::cost_latency(&facts, t));
        findings.extend(criteria::coding_quality(&routing, t));
        findings.extend(criteria::skill_outcomes(&ground_truth, &cite, t));

        let (filed, mut not_filed) = file::file_findings(
            findings,
            &facts,
            &rows,
            self.filer.as_ref(),
            self.ledger.as_ref(),
            cfg,
            now,
        )
        .await;
        skipped.append(&mut not_filed);
        Ok(EvaluationReport {
            at: now,
            metrics: current,
            regressions: regressions.into_iter().map(|r| r.name).collect(),
            filed,
            skipped,
            decisions: Vec::new(),
            probation,
            projection: None,
        })
    }
}
