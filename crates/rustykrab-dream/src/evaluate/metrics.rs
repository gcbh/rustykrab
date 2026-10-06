//! The expectation metrics of plan section 1.1, computed from stored events,
//! and what counts as a regression between two passes.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

use rustykrab_core::proposal::{
    metric_spec, Direction, MetricSlice, MetricValue, ProposalOutcome, AVOIDABLE_ESCALATIONS,
    BOUND_VIOLATIONS, CAPABILITIES_USED, DONE_WITHOUT_INTERVENTION, ESCALATIONS_PER_COMPLETED,
    ESCAPED_DEFECTS, METRICS, PROPOSALS_FILED, PROPOSALS_MOVED_METRIC, RUNGS_PER_ESCALATION,
    TYPED_ERROR_RATE, UNKNOWN_ERROR_RATE, VERIFICATION_GAP,
};
use rustykrab_core::work::{EdgeKind, ErrorClass, Rung, Status, WorkKind};
use rustykrab_store::ProposalRow;

use super::facts::{Facts, RoutingEntry, SurfacedQuestion};

/// What the metrics are computed over.
pub struct MetricInputs<'a> {
    pub facts: &'a Facts,
    pub questions: &'a [SurfacedQuestion],
    pub routing: &'a [RoutingEntry],
    pub proposals: &'a [ProposalRow],
    pub since: DateTime<Utc>,
    pub now: DateTime<Utc>,
    pub window_days: u32,
}

/// A metric's raw counts before it is a [`MetricValue`].
#[derive(Debug, Default)]
struct Tally {
    numerator: f64,
    denominator: f64,
    sample: u32,
    slices: BTreeMap<String, (f64, f64)>,
}

impl Tally {
    fn add(&mut self, scope: Option<String>, hit: bool) {
        let n = if hit { 1.0 } else { 0.0 };
        self.numerator += n;
        self.denominator += 1.0;
        self.sample += 1;
        if let Some(scope) = scope {
            let s = self.slices.entry(scope).or_default();
            s.0 += n;
            s.1 += 1.0;
        }
    }

    fn count(n: u32) -> Tally {
        Tally {
            numerator: f64::from(n),
            denominator: 1.0,
            sample: n,
            slices: BTreeMap::new(),
        }
    }
}

fn ratio(n: f64, d: f64) -> f64 {
    if d > 0.0 && n.is_finite() {
        n / d
    } else {
        0.0
    }
}

fn value(name: &str, t: Tally, inputs: &MetricInputs<'_>) -> MetricValue {
    let spec = metric_spec(name).expect("every computed metric is in METRICS");
    let is_count = spec.unit == "count";
    MetricValue {
        name: name.to_string(),
        expectation: spec.expectation.to_string(),
        direction: spec.direction,
        unit: spec.unit.to_string(),
        signal: spec.signal,
        value: if is_count {
            t.numerator
        } else {
            ratio(t.numerator, t.denominator)
        },
        numerator: t.numerator,
        denominator: if is_count { 1.0 } else { t.denominator },
        sample: t.sample,
        computed_at: inputs.now,
        window_days: inputs.window_days,
        breakdown: t
            .slices
            .into_iter()
            .map(|(scope, (n, d))| MetricSlice {
                scope,
                value: ratio(n, d),
                numerator: n,
                denominator: d,
            })
            .collect(),
    }
}

/// Every metric of section 1.1, in [`METRICS`] order. A metric with
/// nothing to measure is `0` with a sample of `0`, never missing.
pub fn compute(inputs: &MetricInputs<'_>) -> Vec<MetricValue> {
    let f = inputs.facts;
    let since = inputs.since;
    let in_window = |at: Option<DateTime<Utc>>| at.is_some_and(|t| t >= since);
    // The work a user gave: leaves that are not proposals (a proposal's
    // wait for review is by design).
    let leaves: Vec<_> = f
        .items
        .values()
        .filter(|i| i.kind != WorkKind::Proposal && !f.is_parent(&i.id))
        .collect();

    let mut done_unaided = Tally::default();
    for i in leaves.iter().filter(|i| in_window(i.closed_at)) {
        if matches!(i.status, Status::Done | Status::Failed | Status::Expired) {
            let unaided = i.status == Status::Done
                && !f.escalated.contains_key(&i.id)
                && !f.user_touched.contains(&i.id);
            done_unaided.add(Some(format!("kind:{}", i.kind.as_str())), unaided);
        }
    }

    let mut gap = Tally::default();
    let verification_failed: std::collections::BTreeSet<&str> = f
        .failures_of(ErrorClass::Verification)
        .map(|x| x.item.as_str())
        .collect();
    let mut claims: std::collections::BTreeSet<&str> =
        f.claimed.iter().map(String::as_str).collect();
    claims.extend(verification_failed.iter().copied());
    for id in claims {
        let worker = f.worker_of(id).map(|w| format!("worker:{w}"));
        gap.add(worker, verification_failed.contains(id));
    }

    let escaped = Tally::count(inputs.routing.iter().map(|r| r.escaped_defects).sum());

    let mut per_escalation = Tally::default();
    for (id, at) in &f.escalated {
        let before = f
            .rungs
            .get(id)
            .map(|r| {
                r.iter()
                    .filter(|(e, t)| e.rung != Rung::Surface && t <= at)
                    .count()
            })
            .unwrap_or(0);
        per_escalation.numerator += before as f64;
        per_escalation.denominator += 1.0;
        per_escalation.sample += 1;
    }

    let mut escalations = Tally::default();
    let done_leaves = leaves
        .iter()
        .filter(|i| i.status == Status::Done && in_window(i.closed_at))
        .count();
    escalations.numerator = f.escalated.values().filter(|t| **t >= since).count() as f64;
    escalations.denominator = done_leaves as f64;
    escalations.sample = done_leaves as u32;

    let avoidable = Tally::count(
        inputs
            .questions
            .iter()
            .filter(|q| q.is_avoidable())
            .filter(|q| q.answered_at.is_none_or(|t| t >= since))
            .count() as u32,
    );

    let mut typed = Tally::default();
    let mut unknown = Tally::default();
    for x in f.failures.iter().filter(|x| x.at >= since) {
        let kind = f.kind_of(&x.item).map(|k| format!("kind:{}", k.as_str()));
        let is_unknown = x.error.class == ErrorClass::Unknown;
        typed.add(kind.clone(), !is_unknown);
        unknown.add(kind, is_unknown);
    }

    let mut used = Tally::default();
    for cap in f.items.values().filter(|i| {
        i.kind == WorkKind::Capability && i.status == Status::Done && in_window(i.closed_at)
    }) {
        let reused = f
            .dependents_of(&cap.id, EdgeKind::Blocks)
            .iter()
            .any(|e| f.item(&e.item).is_some_and(|d| d.status == Status::Done));
        used.add(None, reused);
    }

    let filed = Tally::count(
        f.items
            .values()
            .filter(|i| i.kind == WorkKind::Proposal && i.created_at >= since)
            .count() as u32,
    );

    let mut moved = Tally::default();
    for p in inputs.proposals.iter().filter(|p| {
        matches!(
            p.outcome,
            ProposalOutcome::Moved | ProposalOutcome::NotMoved | ProposalOutcome::Regressed
        )
    }) {
        moved.add(
            Some(format!("criterion:{}", p.criterion.as_str())),
            p.outcome == ProposalOutcome::Moved,
        );
    }

    let violations = Tally::count(bound_violations(f, since));

    let mut by_name: BTreeMap<&str, Tally> = BTreeMap::new();
    by_name.insert(DONE_WITHOUT_INTERVENTION, done_unaided);
    by_name.insert(VERIFICATION_GAP, gap);
    by_name.insert(ESCAPED_DEFECTS, escaped);
    by_name.insert(RUNGS_PER_ESCALATION, per_escalation);
    by_name.insert(ESCALATIONS_PER_COMPLETED, escalations);
    by_name.insert(AVOIDABLE_ESCALATIONS, avoidable);
    by_name.insert(TYPED_ERROR_RATE, typed);
    by_name.insert(UNKNOWN_ERROR_RATE, unknown);
    by_name.insert(CAPABILITIES_USED, used);
    by_name.insert(PROPOSALS_FILED, filed);
    by_name.insert(PROPOSALS_MOVED_METRIC, moved);
    by_name.insert(BOUND_VIOLATIONS, violations);
    METRICS
        .iter()
        .map(|spec| {
            value(
                spec.name,
                by_name.remove(spec.name).unwrap_or_default(),
                inputs,
            )
        })
        .collect()
}

/// Policy and budget failures, and single-writer conflicts refused at
/// filing, at or after `since`.
fn bound_violations(f: &Facts, since: DateTime<Utc>) -> u32 {
    let failures = f
        .failures
        .iter()
        .filter(|x| x.at >= since)
        .filter(|x| matches!(x.error.class, ErrorClass::Policy | ErrorClass::Budget))
        .count();
    let conflicts = f
        .rejections
        .iter()
        .filter(|(reason, _, at)| reason == "single_writer_conflict" && *at >= since)
        .count();
    (failures + conflicts) as u32
}

/// How far a metric must move, and on how much evidence, before a pass
/// calls it a regression.
#[derive(Debug, Clone, Copy)]
pub struct RegressionRule {
    /// Observations each side of a rate comparison must rest on.
    pub min_sample: u32,
    /// The smallest move of a rate or mean that counts.
    pub tolerance: f64,
}

impl Default for RegressionRule {
    fn default() -> Self {
        RegressionRule {
            min_sample: 5,
            tolerance: 0.05,
        }
    }
}

/// A metric that moved the wrong way since the previous pass.
#[derive(Debug, Clone, PartialEq)]
pub struct Regression {
    pub name: String,
    pub previous: f64,
    pub current: f64,
    /// For a target-zero metric, the occurrences since the previous pass.
    pub fresh: u32,
    pub detail: String,
}

/// New occurrences of a target-zero metric after `at`.
fn fresh_since(name: &str, f: &Facts, at: DateTime<Utc>) -> Option<u32> {
    let after = |class: ErrorClass| f.failures_of(class).filter(|x| x.at > at).count() as u32;
    match name {
        UNKNOWN_ERROR_RATE => Some(after(ErrorClass::Unknown)),
        VERIFICATION_GAP => Some(after(ErrorClass::Verification)),
        BOUND_VIOLATIONS => Some(bound_violations(f, at + chrono::Duration::nanoseconds(1))),
        _ => None,
    }
}

/// The metrics of `current` that moved the wrong way from `previous`.
///
/// A target-zero metric regresses when it is above zero and new
/// occurrences arrived since the previous pass, whatever the rate did: a
/// rate that holds at 1.0 while unknown errors keep arriving has not
/// improved. A rate or mean that should rise or fall regresses when it
/// moved the wrong way by more than the tolerance with enough evidence on
/// both sides. A count that should rise (proposals filed) is reported,
/// not regressed: filing fewer proposals is not a defect to propose on.
pub fn regressions(
    previous: &[MetricValue],
    current: &[MetricValue],
    f: &Facts,
    rule: RegressionRule,
) -> Vec<Regression> {
    let mut out = Vec::new();
    for now in current {
        let Some(before) = previous.iter().find(|p| p.name == now.name) else {
            continue;
        };
        match now.direction {
            Direction::ToZero => {
                let grew = u32::from(now.value > before.value);
                let fresh = fresh_since(&now.name, f, before.computed_at).unwrap_or(grew);
                if now.value > 0.0 && fresh > 0 {
                    out.push(Regression {
                        name: now.name.clone(),
                        previous: before.value,
                        current: now.value,
                        fresh,
                        detail: format!(
                            "{fresh} new occurrence(s) since the previous pass; {} is {:.3}, \
                             was {:.3}, target 0",
                            now.name, now.value, before.value
                        ),
                    });
                }
            }
            Direction::Up | Direction::Down => {
                if now.unit == "count" {
                    continue;
                }
                if now.sample < rule.min_sample || before.sample < rule.min_sample {
                    continue;
                }
                let delta = now.value - before.value;
                let wrong = match now.direction {
                    Direction::Up => delta < -rule.tolerance,
                    _ => delta > rule.tolerance,
                };
                if wrong {
                    out.push(Regression {
                        name: now.name.clone(),
                        previous: before.value,
                        current: now.value,
                        fresh: 0,
                        detail: format!(
                            "{} moved from {:.3} to {:.3} (should go {}) over {} observations",
                            now.name,
                            before.value,
                            now.value,
                            now.direction.as_str(),
                            now.sample
                        ),
                    });
                }
            }
        }
    }
    out
}

/// The current value of a metric a proposal names: a section 1.1 name, or
/// `skill:<name>` for a skill's ground-truth success rate (from
/// `skill_rates`). `None` when it cannot be measured again.
pub fn measure(
    metric: &str,
    current: &[MetricValue],
    skill_rates: &BTreeMap<String, f64>,
) -> Option<f64> {
    if let Some(skill) = metric.strip_prefix("skill:") {
        return skill_rates.get(skill).copied();
    }
    current
        .iter()
        .find(|m| m.name == metric && m.sample > 0)
        .map(|m| m.value)
}
