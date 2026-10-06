//! From findings to filings: the ground-truth gate, the rate and scope
//! limits, the section 10 shape, and what probation concludes about an
//! accepted proposal.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, Utc};

use rustykrab_core::proposal::{
    metric_spec, Criterion, Direction, FiledItem, MetricValue, ProbationResult, ProposalBody,
    ProposalOutcome, ReviewState, SkippedFinding,
};
use rustykrab_core::work::{
    required_review_tier, EdgeKind, PlanOutcome, Status, WorkItemDraft, WorkKind,
};
use rustykrab_store::{ProposalEvidence, ProposalRow};

use super::criteria::{Files, Finding};
use super::facts::Facts;
use super::metrics::measure;
use super::{EvaluationConfig, EvaluationLedger, ProposalFiler};

/// Section 10's source gate: a proposal is filed only from verifiable or
/// explicit evidence; the `internal` item for an unknown error is the one
/// exception, because its evidence is the raw failure itself.
pub fn passes_gate(f: &Finding) -> bool {
    f.signal.is_ground_truth()
        || (f.criterion == Criterion::UnknownError && matches!(f.files, Files::InternalFor(_)))
}

/// The section 10 body of a finding.
pub fn body_of(f: &Finding) -> ProposalBody {
    ProposalBody {
        criterion: f.criterion,
        observed: f.observed.clone(),
        expectation: f.expectation.clone(),
        subject: f.subject.clone(),
        metric: f.metric.clone(),
        expected_movement: f.expected_movement.clone(),
        evidence: f.evidence.clone(),
        counterexamples: f.counterexamples.clone(),
        risk: f.risk.clone(),
        rollback: f.rollback.clone(),
        falsified_by: f.falsified_by.clone(),
        signal: f.signal,
    }
}

fn clip(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(max) {
        Some((cut, _)) => format!("{}...", &flat[..cut]),
        None => flat,
    }
}

/// The `proposal` draft for a finding: the body as the objective, the
/// metric's movement as `done_when`, the rollback condition and the
/// falsifying evaluation as constraints, the evidence as typed pointers,
/// and the tier its subject requires (dreaming never asks below it).
pub fn draft_of(f: &Finding) -> WorkItemDraft {
    let counter = if f.counterexamples.is_empty() {
        "none recorded".to_string()
    } else {
        format!(
            "{} attached to the proposal record",
            f.counterexamples.len()
        )
    };
    let objective = [
        format!("Observed: {}.", f.observed.trim_end_matches('.')),
        format!("Expectation served: {}.", f.expectation),
        format!("Would change: {}.", f.subject),
        format!(
            "Metric: {}; expected movement: {}.",
            f.metric, f.expected_movement
        ),
        format!(
            "Evidence: {} reference(s) ({} signal); counterexamples: {counter}.",
            f.evidence.len(),
            f.signal.as_str()
        ),
        format!("Risk: {}", f.risk),
        format!("Rollback: {}", f.rollback),
        format!("Falsified by: {}", f.falsified_by),
        format!("Criterion: {} (plan section 10).", f.criterion.as_str()),
    ]
    .join("\n");
    WorkItemDraft {
        kind: Some(WorkKind::Proposal),
        title: clip(&f.title, 180),
        objective,
        done_when: format!(
            "{} moves as expected ({}) on replay or within the probation window.",
            f.metric, f.expected_movement
        ),
        constraints: vec![
            format!("Rollback: {}", f.rollback),
            format!("Falsified by: {}", f.falsified_by),
        ],
        artifact_refs: f.evidence.clone(),
        subject: Some(f.subject.clone()),
        review_tier: Some(required_review_tier(Some(&f.subject))),
        ..WorkItemDraft::default()
    }
}

fn evidence_rows(f: &Finding) -> Vec<ProposalEvidence> {
    f.evidence
        .iter()
        .map(|r| ProposalEvidence {
            source: match r.kind.as_str() {
                "item" => "work_item".to_string(),
                other => other.to_string(),
            },
            reference: r.value.clone(),
        })
        .collect()
}

/// The subjects a new proposal may not take (section 10: one open proposal
/// per subject): an open proposal's, an accepted one still in probation,
/// and one declined inside the cool-down.
pub fn live_subjects(
    facts: &Facts,
    rows: &[ProposalRow],
    now: DateTime<Utc>,
    cooldown: Duration,
) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for item in facts.items.values() {
        if item.kind == WorkKind::Proposal && !item.status.is_closed() {
            if let Some(subject) = facts.facets.get(&item.id).and_then(|f| f.subject.clone()) {
                out.insert(subject);
            }
        }
    }
    for row in rows {
        let open_row = facts.item(&row.item).is_none_or(|i| !i.status.is_closed())
            && row.review == ReviewState::Pending;
        let probation = row.review == ReviewState::Accepted && !row.outcome.is_final();
        let cooling = row.review == ReviewState::Declined
            && row.decided_at.is_some_and(|at| at > now - cooldown);
        if open_row || probation || cooling {
            out.insert(row.subject.clone());
        }
    }
    out
}

/// File what passes the gate and the limits; report the rest.
pub async fn file_findings(
    findings: Vec<Finding>,
    facts: &Facts,
    rows: &[ProposalRow],
    filer: &dyn ProposalFiler,
    ledger: &dyn EvaluationLedger,
    config: &EvaluationConfig,
    now: DateTime<Utc>,
) -> (Vec<FiledItem>, Vec<SkippedFinding>) {
    let mut filed = Vec::new();
    let mut skipped = Vec::new();
    let mut live = live_subjects(facts, rows, now, config.decline_cooldown);
    let mut today = rows
        .iter()
        .filter(|r| r.filed_by == config.actor && r.created_at > now - Duration::hours(24))
        .count() as u32;
    let mut skip = |f: &Finding, why: String| {
        skipped.push(SkippedFinding {
            criterion: f.criterion,
            subject: f.subject.clone(),
            why,
        })
    };
    for finding in findings {
        if !passes_gate(&finding) {
            skip(
                &finding,
                format!(
                    "{} evidence only: the ground-truth gate files nothing from it",
                    finding.signal.as_str()
                ),
            );
            continue;
        }
        match &finding.files {
            Files::InternalFor(error) => {
                if !live.insert(finding.subject.clone()) {
                    continue;
                }
                match filer.file_internal(error, finding.evidence.clone()).await {
                    Ok(PlanOutcome::Accepted(a)) => filed.push(FiledItem {
                        id: a.root,
                        kind: WorkKind::Internal,
                        criterion: finding.criterion,
                        subject: finding.subject.clone(),
                        title: finding.title.clone(),
                    }),
                    Ok(PlanOutcome::Rejected(r)) => skip(
                        &finding,
                        format!("the controller refused it: {:?}", reasons(&r)),
                    ),
                    Err(e) => skip(&finding, format!("filing failed: {e}")),
                }
            }
            Files::Proposal => {
                if live.contains(&finding.subject) {
                    skip(
                        &finding,
                        "a live proposal on this subject exists (one per subject)".to_string(),
                    );
                    continue;
                }
                if today >= config.proposals_per_day {
                    skip(
                        &finding,
                        format!(
                            "rate limit: {} proposals filed in the last day",
                            config.proposals_per_day
                        ),
                    );
                    continue;
                }
                let draft = draft_of(&finding);
                match filer.file(draft.clone()).await {
                    Ok(PlanOutcome::Accepted(a)) => {
                        let row = ProposalRow {
                            item: a.root.clone(),
                            subject: finding.subject.clone(),
                            criterion: finding.criterion,
                            metric: finding.metric.clone(),
                            body: body_of(&finding),
                            filed_by: config.actor.clone(),
                            created_at: now,
                            review: ReviewState::Pending,
                            decided_by: None,
                            decided_at: None,
                            code_item: None,
                            baseline: None,
                            outcome: ProposalOutcome::Pending,
                            observed: None,
                            outcome_at: None,
                        };
                        if let Err(e) = ledger.record_proposal(&row, &evidence_rows(&finding)).await
                        {
                            tracing::warn!(error = %e, proposal = %a.root,
                                "proposal filed but its record was not written");
                        }
                        live.insert(finding.subject.clone());
                        today += 1;
                        filed.push(FiledItem {
                            id: a.root,
                            kind: WorkKind::Proposal,
                            criterion: finding.criterion,
                            subject: finding.subject.clone(),
                            title: draft.title,
                        });
                    }
                    Ok(PlanOutcome::Rejected(r)) => skip(
                        &finding,
                        format!("the controller refused it: {:?}", reasons(&r)),
                    ),
                    Err(e) => skip(&finding, format!("filing failed: {e}")),
                }
            }
        }
    }
    (filed, skipped)
}

fn reasons(r: &rustykrab_core::work::PlanRejected) -> Vec<&'static str> {
    r.failed.iter().map(|c| c.reason.as_str()).collect()
}

/// Which way is better for a metric a proposal names.
fn better_direction(metric: &str) -> Direction {
    if metric.starts_with("skill:") {
        return Direction::Up;
    }
    metric_spec(metric)
        .map(|m| m.direction)
        .unwrap_or(Direction::Up)
}

/// Whether `observed` moved from `baseline` the way the proposal wanted.
pub fn verdict(metric: &str, baseline: f64, observed: f64, tolerance: f64) -> ProposalOutcome {
    let delta = observed - baseline;
    let improved = match better_direction(metric) {
        Direction::Up => delta > tolerance,
        Direction::Down | Direction::ToZero => delta < -tolerance,
    };
    let worsened = match better_direction(metric) {
        Direction::Up => delta < -tolerance,
        Direction::Down | Direction::ToZero => delta > tolerance,
    };
    if improved {
        ProposalOutcome::Moved
    } else if worsened {
        ProposalOutcome::Regressed
    } else {
        ProposalOutcome::NotMoved
    }
}

/// Bring each proposal record up to date with its item, then run
/// probation: a decision taken on the review surface since the last pass
/// is recorded (an acceptance with the metric's value as its baseline),
/// and an accepted proposal whose `code` item landed a probation window
/// ago is judged against that baseline. Returns the probation verdicts
/// and the proposals whose metric regressed, for a rollback proposal.
pub async fn reconcile(
    facts: &Facts,
    rows: &mut [ProposalRow],
    current: &[MetricValue],
    skill_rates: &BTreeMap<String, f64>,
    ledger: &dyn EvaluationLedger,
    config: &EvaluationConfig,
    now: DateTime<Utc>,
) -> (Vec<ProbationResult>, Vec<ProposalRow>) {
    let mut results = Vec::new();
    let mut regressed = Vec::new();
    for row in rows.iter_mut() {
        if row.review == ReviewState::Pending {
            let Some(item) = facts.item(&row.item) else {
                continue;
            };
            let review = match item.status {
                Status::Done => ReviewState::Accepted,
                s if s.is_closed() => ReviewState::Declined,
                _ => continue,
            };
            let code = facts
                .dependents_of(&row.item, EdgeKind::DiscoveredFrom)
                .into_iter()
                .find(|e| facts.kind_of(&e.item) == Some(WorkKind::Code))
                .map(|e| e.item.clone());
            let baseline = (review == ReviewState::Accepted)
                .then(|| measure(&row.metric, current, skill_rates))
                .flatten();
            let decided_at = item.closed_at.unwrap_or(now);
            if let Err(e) = ledger
                .record_review(
                    &row.item,
                    review,
                    "review surface",
                    decided_at,
                    code.as_deref(),
                    baseline,
                )
                .await
            {
                tracing::warn!(error = %e, proposal = %row.item, "could not record a review");
                continue;
            }
            row.review = review;
            row.decided_at = Some(decided_at);
            row.code_item = code;
            row.baseline = baseline;
        }
        if row.review != ReviewState::Accepted || row.outcome.is_final() {
            continue;
        }
        let Some(code) = row.code_item.as_deref().and_then(|c| facts.item(c)) else {
            continue;
        };
        let outcome_and_value = match code.status {
            Status::Done => {
                let landed = code.closed_at.unwrap_or(now);
                if now - landed < config.probation {
                    continue;
                }
                match (row.baseline, measure(&row.metric, current, skill_rates)) {
                    (Some(b), Some(o)) => (
                        verdict(&row.metric, b, o, config.regression.tolerance),
                        Some(o),
                    ),
                    _ => (ProposalOutcome::Unmeasurable, None),
                }
            }
            // The change never landed, so it moved nothing.
            s if s.is_closed() => (ProposalOutcome::NotMoved, None),
            _ => continue,
        };
        let (outcome, observed) = outcome_and_value;
        if let Err(e) = ledger
            .record_outcome(&row.item, outcome, observed, now)
            .await
        {
            tracing::warn!(error = %e, proposal = %row.item, "could not record an outcome");
            continue;
        }
        row.outcome = outcome;
        row.observed = observed;
        results.push(ProbationResult {
            proposal: row.item.clone(),
            metric: row.metric.clone(),
            outcome,
            baseline: row.baseline,
            observed,
        });
        if outcome == ProposalOutcome::Regressed {
            regressed.push(row.clone());
        }
    }
    (results, regressed)
}

/// The rollback proposal for an accepted change whose metric regressed
/// through probation: back through review, never undone on its own.
pub fn rollback_finding(row: &ProposalRow) -> Finding {
    let code = row.code_item.clone().unwrap_or_default();
    Finding {
        criterion: row.criterion,
        files: Files::Proposal,
        subject: row.subject.clone(),
        title: format!(
            "Roll back the change for {}: {} regressed through probation",
            row.subject, row.metric
        ),
        observed: format!(
            "{} was {:.3} when the proposal was accepted and {:.3} after probation",
            row.metric,
            row.baseline.unwrap_or_default(),
            row.observed.unwrap_or_default()
        ),
        expectation: row.body.expectation.clone(),
        metric: row.metric.clone(),
        expected_movement: format!("{} returns to its baseline", row.metric),
        evidence: vec![
            rustykrab_core::work::ArtifactRef {
                kind: "item".to_string(),
                value: row.item.clone(),
            },
            rustykrab_core::work::ArtifactRef {
                kind: "item".to_string(),
                value: code,
            },
        ],
        counterexamples: Vec::new(),
        risk: "Reverting also reverts whatever the change got right.".to_string(),
        rollback: "Re-apply the change if the metric does not recover.".to_string(),
        falsified_by: format!("{} not recovering after the revert.", row.metric),
        signal: rustykrab_core::outcome::SignalClass::Verifiable,
    }
}
