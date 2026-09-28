//! Where the evaluation looks (plan section 10's table): each criterion
//! reads its evidence and returns findings, each naming what it would file.
//! Nothing here files anything or applies the gate; that is `file.rs`.

use std::collections::{BTreeMap, BTreeSet};

use rustykrab_core::outcome::{OutcomeTally, SignalClass};
use rustykrab_core::proposal::{metric_spec, Criterion, BOUND_VIOLATIONS};
use rustykrab_core::work::{
    ArtifactRef, CapabilityMode, EdgeKind, ErrorClass, Rung, Status, WorkError, WorkKind,
};

use super::facts::{Facts, RoutingEntry, SurfacedQuestion};
use super::metrics::Regression;

/// What a finding files.
#[derive(Debug, Clone, PartialEq)]
pub enum Files {
    /// A `proposal` for a human to review.
    Proposal,
    /// The `internal` item for an error (the controller's draft shape).
    InternalFor(Box<WorkError>),
}

/// One place the system could improve, with its evidence.
#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub criterion: Criterion,
    pub files: Files,
    /// What it would change, `<area>` or `<area>:<name>`; one live
    /// proposal per subject.
    pub subject: String,
    /// One line, safe for the review surface: never another item's title.
    pub title: String,
    pub observed: String,
    pub expectation: String,
    /// The metric expected to move.
    pub metric: String,
    pub expected_movement: String,
    pub evidence: Vec<ArtifactRef>,
    pub counterexamples: Vec<ArtifactRef>,
    pub risk: String,
    pub rollback: String,
    pub falsified_by: String,
    /// The class of evidence the finding rests on.
    pub signal: SignalClass,
}

/// How much a criterion needs before it speaks.
#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    /// Observations a rate must rest on.
    pub min_sample: u32,
    /// Items a fingerprint must recur across.
    pub recurring: usize,
    /// A skill at or below this ground-truth success rate is flagged.
    pub skill_rate_at_most: f64,
    /// A worker whose claims fail verification this often is flagged.
    pub verification_miss_rate: f64,
    /// A routing class moves when the default's verified rate is below
    /// `routing_poor` and another worker's is at least `routing_good`.
    pub routing_poor: f64,
    pub routing_good: f64,
    /// Occurrences before a plan-shape or wasted-rung pattern is flagged.
    pub pattern: usize,
    /// How much slower a worker must be than the fastest on the same kind.
    pub latency_factor: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds {
            min_sample: 5,
            recurring: 3,
            skill_rate_at_most: 0.5,
            verification_miss_rate: 0.5,
            routing_poor: 0.5,
            routing_good: 0.8,
            pattern: 3,
            latency_factor: 2.0,
        }
    }
}

fn item_ref(id: &str) -> ArtifactRef {
    ArtifactRef {
        kind: "item".to_string(),
        value: id.to_string(),
    }
}

fn r(kind: &str, value: impl Into<String>) -> ArtifactRef {
    ArtifactRef {
        kind: kind.to_string(),
        value: value.into(),
    }
}

const MAX_EVIDENCE: usize = 20;

fn capped(mut refs: Vec<ArtifactRef>) -> Vec<ArtifactRef> {
    refs.dedup();
    refs.truncate(MAX_EVIDENCE);
    refs
}

/// Expectation regressions: a section 1.1 metric moving the wrong way.
pub fn expectation_regressions(regressions: &[Regression]) -> Vec<Finding> {
    regressions
        .iter()
        .map(|g| {
            let spec = metric_spec(&g.name);
            let expectation = spec.map(|s| s.expectation).unwrap_or("").to_string();
            let direction = spec.map(|s| s.direction.as_str()).unwrap_or("up");
            // Bounds are policy: a proposal to change them is reviewed at
            // the highest tier (section 10).
            let subject = if g.name == BOUND_VIOLATIONS {
                "policy:bounds".to_string()
            } else {
                format!("expectation:{}", g.name)
            };
            Finding {
                criterion: Criterion::ExpectationRegression,
                files: Files::Proposal,
                subject,
                title: if g.fresh > 0 {
                    format!(
                        "Expectation regression: {} at {:.3} with {} new occurrence(s) since \
                         the last pass",
                        g.name, g.current, g.fresh
                    )
                } else {
                    format!(
                        "Expectation regression: {} moved from {:.3} to {:.3}",
                        g.name, g.previous, g.current
                    )
                },
                observed: g.detail.clone(),
                expectation,
                metric: g.name.clone(),
                expected_movement: format!("{} goes {direction} again", g.name),
                evidence: vec![r("metric", g.name.clone())],
                counterexamples: Vec::new(),
                risk: "A fix aimed at the metric can move a neighbouring one the other way."
                    .to_string(),
                rollback: format!(
                    "Revert the change if {} has not moved back within the probation window.",
                    g.name
                ),
                falsified_by: format!(
                    "The next passes' {} staying where it is after the change lands.",
                    g.name
                ),
                signal: SignalClass::Verifiable,
            }
        })
        .collect()
}

/// Avoidable escalations: a question that reached the user and was
/// answered with its recorded default (a lower rung could have answered
/// it). Explicit: the user's own answer is the evidence.
pub fn avoidable_escalations(questions: &[SurfacedQuestion]) -> Vec<Finding> {
    let mut by_class: BTreeMap<&str, Vec<&SurfacedQuestion>> = BTreeMap::new();
    for q in questions.iter().filter(|q| q.is_avoidable()) {
        by_class.entry(q.class.as_str()).or_default().push(q);
    }
    by_class
        .into_iter()
        .map(|(class, qs)| {
            let mut evidence = Vec::new();
            for q in &qs {
                evidence.push(item_ref(&q.item));
                evidence.push(r("question", q.id.clone()));
            }
            let class = if class.trim().is_empty() {
                "unclassified"
            } else {
                class
            };
            Finding {
                criterion: Criterion::AvoidableEscalation,
                files: Files::Proposal,
                subject: format!("policy:defaults:{class}"),
                title: format!(
                    "Avoidable escalation: {} `{class}` question(s) answered with the recorded \
                     default",
                    qs.len()
                ),
                observed: format!(
                    "{} question(s) of class `{class}` reached the user and were answered with \
                     the default already recorded for them, so the escalation was avoidable: \
                     standing judgment could have answered.",
                    qs.len()
                ),
                expectation: "Surface rarely, and only when stuck".to_string(),
                metric: rustykrab_core::proposal::AVOIDABLE_ESCALATIONS.to_string(),
                expected_movement: "avoidable_escalations falls to zero for this class".to_string(),
                evidence: capped(evidence),
                counterexamples: Vec::new(),
                risk: "A default applied where the user would have chosen otherwise.".to_string(),
                rollback: "Withdraw the default if the user overrides it once.".to_string(),
                falsified_by: "The user choosing something other than the default on the next \
                               questions of this class."
                    .to_string(),
                signal: SignalClass::Explicit,
            }
        })
        .collect()
}

/// Whether an `internal` item for `fingerprint` already exists (the
/// controller's titles carry the fingerprint).
fn has_internal_for(f: &Facts, fingerprint: &str) -> bool {
    f.items
        .values()
        .any(|i| i.kind == WorkKind::Internal && i.title.contains(fingerprint))
}

/// Unknown errors: every `unknown` fingerprint with no `internal` item yet
/// files one (observability), gate or no gate.
pub fn unknown_errors(f: &Facts) -> Vec<Finding> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for x in f.failures_of(ErrorClass::Unknown) {
        if !seen.insert(x.error.fingerprint.clone()) || has_internal_for(f, &x.error.fingerprint) {
            continue;
        }
        out.push(Finding {
            criterion: Criterion::UnknownError,
            files: Files::InternalFor(Box::new(x.error.clone())),
            subject: format!("fingerprint:{}", x.error.fingerprint),
            title: format!("Classify unknown failure {}", x.error.fingerprint),
            observed: x.error.detail.clone(),
            expectation: "Know what went wrong".to_string(),
            metric: rustykrab_core::proposal::UNKNOWN_ERROR_RATE.to_string(),
            expected_movement: "unknown_error_rate falls".to_string(),
            evidence: vec![item_ref(&x.item)],
            counterexamples: Vec::new(),
            risk: String::new(),
            rollback: String::new(),
            falsified_by: String::new(),
            // The evidence is the raw failure itself (section 10's one
            // exception to the gate).
            signal: SignalClass::Implicit,
        });
    }
    out
}

/// Recurring fingerprints: the same known error across items files the
/// fix-the-cause `internal` item.
pub fn recurring_fingerprints(f: &Facts, t: Thresholds) -> Vec<Finding> {
    let mut by_fp: BTreeMap<&str, Vec<&super::facts::Failure>> = BTreeMap::new();
    for x in &f.failures {
        if x.error.class != ErrorClass::Unknown {
            by_fp
                .entry(x.error.fingerprint.as_str())
                .or_default()
                .push(x);
        }
    }
    by_fp
        .into_iter()
        .filter(|(fp, xs)| xs.len() >= t.recurring && !has_internal_for(f, fp))
        .map(|(fp, xs)| {
            let first = xs[0];
            // A model's diagnosis is judgement, not ground truth.
            let signal = if first.error.observed_by == "diagnosis" {
                SignalClass::Judge
            } else {
                SignalClass::Verifiable
            };
            Finding {
                criterion: Criterion::RecurringFingerprint,
                files: Files::InternalFor(Box::new(first.error.clone())),
                subject: format!("fingerprint:{fp}"),
                title: format!(
                    "Stop recurring {}/{} failure {fp}",
                    first.error.class.as_str(),
                    first.error.subclass.as_str()
                ),
                observed: format!("seen on {} items", xs.len()),
                expectation: "Know what went wrong".to_string(),
                metric: rustykrab_core::proposal::TYPED_ERROR_RATE.to_string(),
                expected_movement: "the fingerprint stops recurring".to_string(),
                evidence: capped(xs.iter().map(|x| item_ref(&x.item)).collect()),
                counterexamples: Vec::new(),
                risk: String::new(),
                rollback: String::new(),
                falsified_by: String::new(),
                signal,
            }
        })
        .collect()
}

/// Capability gaps: a capability item more than one item waited on. A
/// build that served several items belongs in a definition's default set;
/// an acquisition several items needed should be made ahead of time.
pub fn capability_gaps(f: &Facts, t: Thresholds) -> Vec<Finding> {
    let mut out = Vec::new();
    for cap in f.items.values().filter(|i| i.kind == WorkKind::Capability) {
        let waiting: Vec<_> = f.dependents_of(&cap.id, EdgeKind::Blocks);
        if waiting.len() < 2.max(t.pattern.saturating_sub(1)) {
            continue;
        }
        let mode = f
            .facets
            .get(&cap.id)
            .and_then(|x| x.capability)
            .unwrap_or(CapabilityMode::Acquire);
        let mut evidence = vec![item_ref(&cap.id)];
        evidence.extend(waiting.iter().map(|e| item_ref(&e.item)));
        let (subject, title, observed) = match mode {
            CapabilityMode::Build if cap.status == Status::Done => (
                format!("agent_definition:tools:{}", cap.id),
                format!(
                    "Capability gap: add a built capability to a definition's default set ({} \
                     items needed it)",
                    waiting.len()
                ),
                "a capability the ladder built was then needed by several items".to_string(),
            ),
            _ => (
                format!("capability:{}", cap.id),
                format!(
                    "Capability gap: acquire ahead of time what {} items waited on",
                    waiting.len()
                ),
                "several items parked on the same missing capability".to_string(),
            ),
        };
        out.push(Finding {
            criterion: Criterion::CapabilityGap,
            files: Files::Proposal,
            subject,
            title,
            observed,
            expectation: "Grow its own capability".to_string(),
            metric: rustykrab_core::proposal::CAPABILITIES_USED.to_string(),
            expected_movement: "fewer items park on needs_tool for this capability".to_string(),
            evidence: capped(evidence),
            counterexamples: Vec::new(),
            risk: "A larger starting tool set costs prefill on every run that loads it."
                .to_string(),
            rollback: "Remove it from the default set if runs that load it do not use it."
                .to_string(),
            falsified_by: "needs_tool parks for the capability continuing after it lands."
                .to_string(),
            signal: SignalClass::Verifiable,
        });
    }
    out
}

/// Wasted rungs and plan shape: retries on errors that are not transient,
/// repairs that never changed the outcome, and `work_plan` rejections and
/// `sequential_split` warnings by reason.
pub fn wasted_rungs(f: &Facts, t: Thresholds) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut futile_retries: Vec<&str> = Vec::new();
    let mut futile_repairs: Vec<&str> = Vec::new();
    for (id, rungs) in &f.rungs {
        let retried_hard = rungs.iter().any(|(e, _)| {
            e.rung == Rung::Retry && e.error.as_ref().is_some_and(|x| !x.subclass.is_transient())
        });
        if retried_hard {
            futile_retries.push(id);
        }
        let repairs = rungs.iter().filter(|(e, _)| e.rung == Rung::Repair).count();
        if repairs >= 2 && f.item(id).is_some_and(|i| i.status == Status::Failed) {
            futile_repairs.push(id);
        }
    }
    let ladder = |criterion_subject: &str, n: usize, what: &str, ids: &[&str]| Finding {
        criterion: Criterion::WastedRungs,
        files: Files::Proposal,
        subject: criterion_subject.to_string(),
        title: format!("Wasted rungs: {n} items {what}"),
        observed: format!("{n} items {what}"),
        expectation: "Persist before surfacing".to_string(),
        metric: rustykrab_core::proposal::RUNGS_PER_ESCALATION.to_string(),
        expected_movement: "rungs spent on a failure change its outcome more often".to_string(),
        evidence: capped(ids.iter().map(|id| item_ref(id)).collect()),
        counterexamples: Vec::new(),
        risk: "A tighter ladder surfaces sooner on a failure a retry would have fixed.".to_string(),
        rollback: "Restore the budget if escalations per completed item rise.".to_string(),
        falsified_by: "The same pattern recurring at the same rate after the change.".to_string(),
        signal: SignalClass::Verifiable,
    };
    if futile_retries.len() >= t.pattern {
        out.push(ladder(
            "ladder_budgets:retries",
            futile_retries.len(),
            "retried an error that is not transient",
            &futile_retries,
        ));
    }
    if futile_repairs.len() >= t.pattern {
        out.push(ladder(
            "ladder_budgets:repairs",
            futile_repairs.len(),
            "spent two or more repairs and failed anyway",
            &futile_repairs,
        ));
    }
    let mut by_reason: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (reason, item, _) in &f.rejections {
        by_reason
            .entry(reason.as_str())
            .or_default()
            .push(item.as_str());
    }
    for (item, _) in &f.split_warnings {
        by_reason
            .entry("sequential_split")
            .or_default()
            .push(item.as_str());
    }
    for (reason, items) in by_reason {
        if items.len() < t.pattern || reason == "single_writer_conflict" {
            continue;
        }
        out.push(Finding {
            criterion: Criterion::WastedRungs,
            files: Files::Proposal,
            subject: format!("agent_definition:planner:{reason}"),
            title: format!(
                "Plan shape: {} filings refused or warned with {reason}",
                items.len()
            ),
            observed: format!("{} work_plan outcomes carried {reason}", items.len()),
            expectation: "Finish what it is given".to_string(),
            metric: rustykrab_core::proposal::DONE_WITHOUT_INTERVENTION.to_string(),
            expected_movement: format!("fewer {reason} outcomes per planning run"),
            evidence: capped(items.iter().map(|id| item_ref(id)).collect()),
            counterexamples: Vec::new(),
            risk: "A planner prompt tuned against one reason can trip another.".to_string(),
            rollback: "Revert the planner definition if rejections of any reason rise.".to_string(),
            falsified_by: format!("{reason} outcomes continuing at the same rate."),
            signal: SignalClass::Verifiable,
        });
    }
    out
}

/// Verification misses: a worker whose claims fail verification often.
pub fn verification_misses(f: &Facts, t: Thresholds) -> Vec<Finding> {
    let failed: BTreeSet<&str> = f
        .failures_of(ErrorClass::Verification)
        .map(|x| x.item.as_str())
        .collect();
    let mut by_worker: BTreeMap<&str, (Vec<&str>, Vec<&str>)> = BTreeMap::new();
    let mut claims: BTreeSet<&str> = f.claimed.iter().map(String::as_str).collect();
    claims.extend(failed.iter().copied());
    for id in claims {
        let Some(worker) = f.worker_of(id) else {
            continue;
        };
        let e = by_worker.entry(worker).or_default();
        if failed.contains(id) {
            e.0.push(id);
        } else {
            e.1.push(id);
        }
    }
    by_worker
        .into_iter()
        .filter_map(|(worker, (misses, good))| {
            let total = misses.len() + good.len();
            let rate = misses.len() as f64 / total.max(1) as f64;
            (total >= t.min_sample as usize && rate >= t.verification_miss_rate).then(|| Finding {
                criterion: Criterion::VerificationMiss,
                files: Files::Proposal,
                subject: format!("brief:{worker}"),
                title: format!(
                    "Verification misses: {} of {total} claims by {worker} failed verification",
                    misses.len()
                ),
                observed: format!(
                    "{worker}'s results claimed more than their evidence showed {:.0}% of the \
                     time",
                    rate * 100.0
                ),
                expectation: "Finish it correctly".to_string(),
                metric: rustykrab_core::proposal::VERIFICATION_GAP.to_string(),
                expected_movement: format!("verification_gap_rate for worker:{worker} falls"),
                evidence: capped(misses.iter().map(|id| item_ref(id)).collect()),
                counterexamples: capped(good.iter().map(|id| item_ref(id)).collect()),
                risk: "A stricter brief can slow the worker on work it already did well."
                    .to_string(),
                rollback: "Revert the brief if the worker's verified rate drops.".to_string(),
                falsified_by: "The miss rate holding after the brief changes.".to_string(),
                signal: SignalClass::Verifiable,
            })
        })
        .collect()
}

/// Cost and latency: wall time per completed item by worker, against the
/// fastest worker that completed the same kind of item.
pub fn cost_latency(f: &Facts, t: Thresholds) -> Vec<Finding> {
    let mut by_kind: BTreeMap<&'static str, BTreeMap<&str, Vec<f64>>> = BTreeMap::new();
    for i in f.items.values().filter(|i| i.status == Status::Done) {
        let (Some(start), Some(end), Some(worker)) =
            (f.leased_at.get(&i.id), i.closed_at, f.worker_of(&i.id))
        else {
            continue;
        };
        let secs = (end - *start).num_milliseconds().max(0) as f64 / 1000.0;
        by_kind
            .entry(i.kind.as_str())
            .or_default()
            .entry(worker)
            .or_default()
            .push(secs);
    }
    let mut out = Vec::new();
    for (kind, workers) in by_kind {
        let means: Vec<(&str, f64, usize)> = workers
            .iter()
            .filter(|(_, xs)| xs.len() >= t.min_sample as usize)
            .map(|(w, xs)| (*w, xs.iter().sum::<f64>() / xs.len() as f64, xs.len()))
            .collect();
        let Some(fastest) = means
            .iter()
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        else {
            continue;
        };
        for (worker, mean, n) in &means {
            if *worker == fastest.0 || *mean < fastest.1 * t.latency_factor {
                continue;
            }
            out.push(Finding {
                criterion: Criterion::CostLatency,
                files: Files::Proposal,
                subject: format!("routing:{kind}:{worker}"),
                title: format!(
                    "Cost and latency: {worker} takes {:.0}s per {kind} item, {} takes {:.0}s",
                    mean, fastest.0, fastest.1
                ),
                observed: format!(
                    "{worker} averaged {mean:.1}s over {n} completed {kind} items; {} averaged \
                     {:.1}s over {}",
                    fastest.0, fastest.1, fastest.2
                ),
                expectation: "Finish what it is given".to_string(),
                metric: rustykrab_core::proposal::DONE_WITHOUT_INTERVENTION.to_string(),
                expected_movement: format!("wall time per {kind} item falls"),
                evidence: vec![r("worker", *worker), r("worker", fastest.0)],
                counterexamples: Vec::new(),
                risk: "The faster worker may be faster because it took easier items.".to_string(),
                rollback: "Route back if the faster worker's verified rate drops.".to_string(),
                falsified_by: "The gap closing without a routing change.".to_string(),
                signal: SignalClass::Verifiable,
            });
        }
    }
    out
}

/// Coding quality by worker: a routing proposal when a class's default
/// tier should move, citing both workers' records (the dreaming half of
/// scenario 17). The controller never moves a default on its own.
pub fn coding_quality(routing: &[RoutingEntry], t: Thresholds) -> Vec<Finding> {
    let mut by_class: BTreeMap<&str, Vec<&RoutingEntry>> = BTreeMap::new();
    for e in routing {
        by_class.entry(e.class.as_str()).or_default().push(e);
    }
    let mut out = Vec::new();
    for (class, entries) in by_class {
        let Some(default) = entries.iter().find(|e| e.default_for_class) else {
            continue;
        };
        let enough = |e: &RoutingEntry| e.claims() >= t.min_sample;
        let rate = |e: &RoutingEntry| e.verified_rate().unwrap_or(0.0);
        // Up: the default fails and another worker holds the class.
        // Down: a cheaper worker holds it as well as the default does.
        let better = entries
            .iter()
            .filter(|e| e.worker != default.worker && enough(e) && rate(e) >= t.routing_good)
            .filter(|e| {
                (enough(default) && rate(default) < t.routing_poor)
                    || (e.cost_tier < default.cost_tier && rate(e) >= rate(default))
            })
            .min_by_key(|e| e.cost_tier);
        let Some(to) = better else {
            continue;
        };
        let direction = if to.cost_tier < default.cost_tier {
            "down"
        } else {
            "up"
        };
        out.push(Finding {
            criterion: Criterion::CodingQuality,
            files: Files::Proposal,
            subject: format!("routing:{class}"),
            title: format!(
                "Routing: move the default for {class} {direction} from {} to {}",
                default.worker, to.worker
            ),
            observed: format!(
                "{} verified {} of {} claims on {class}; {} verified {} of {}",
                default.worker,
                default.verified_done,
                default.claims(),
                to.worker,
                to.verified_done,
                to.claims()
            ),
            expectation: "Finish it correctly".to_string(),
            metric: rustykrab_core::proposal::VERIFICATION_GAP.to_string(),
            expected_movement: format!("verified rate on {class} holds or rises at lower cost"),
            evidence: vec![
                r("routing_record", format!("{}:{class}", default.worker)),
                r("routing_record", format!("{}:{class}", to.worker)),
            ],
            counterexamples: Vec::new(),
            risk: "The class's items may differ from those the record was earned on.".to_string(),
            rollback: format!(
                "Move the default back to {} if {}'s verified rate on {class} falls below {:.0}%.",
                default.worker,
                to.worker,
                t.routing_poor * 100.0
            ),
            falsified_by: format!("{}'s verified rate on {class} under probation.", to.worker),
            signal: SignalClass::Verifiable,
        });
    }
    out
}

/// Skill outcomes: a skill whose ground-truth record is poor gets a skill
/// delta proposal. `ground_truth` must hold only verifiable and explicit
/// tallies; `cite` gives record ids per skill, failures then successes.
pub fn skill_outcomes(
    ground_truth: &[(String, OutcomeTally)],
    cite: &BTreeMap<String, (Vec<String>, Vec<String>)>,
    t: Thresholds,
) -> Vec<Finding> {
    ground_truth
        .iter()
        .filter_map(|(skill, tally)| {
            let rate = tally.success_rate(t.min_sample)?;
            (rate <= t.skill_rate_at_most).then(|| {
                let (fails, wins) = cite.get(skill).cloned().unwrap_or_default();
                Finding {
                    criterion: Criterion::SkillOutcome,
                    files: Files::Proposal,
                    subject: format!("skill:{skill}"),
                    title: format!(
                        "Skill outcome: {skill} failed {} of {} verified runs",
                        tally.harmful,
                        tally.decisive()
                    ),
                    observed: format!(
                        "skill {skill} succeeded on {:.0}% of {} runs judged on verifiable or \
                         explicit evidence",
                        rate * 100.0,
                        tally.decisive()
                    ),
                    expectation: "Finish it correctly".to_string(),
                    metric: format!("skill:{skill}"),
                    expected_movement: format!(
                        "skill {skill}'s ground-truth success rate rises above {:.0}%",
                        t.skill_rate_at_most * 100.0
                    ),
                    evidence: capped(
                        fails
                            .into_iter()
                            .map(|id| r("outcome_record", id))
                            .collect(),
                    ),
                    counterexamples: capped(
                        wins.into_iter().map(|id| r("outcome_record", id)).collect(),
                    ),
                    risk: "A skill delta can regress the runs it already got right.".to_string(),
                    rollback: format!(
                        "Revert the skill delta if skill {skill}'s success rate falls below \
                         {:.0}% over the probation window.",
                        rate * 100.0
                    ),
                    falsified_by: format!(
                        "The next {} verified runs of {skill} failing at the same rate.",
                        t.min_sample
                    ),
                    signal: SignalClass::Verifiable,
                }
            })
        })
        .collect()
}
