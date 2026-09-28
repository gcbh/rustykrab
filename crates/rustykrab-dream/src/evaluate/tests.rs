//! The evaluation pass against in-memory sources: the dreaming halves of
//! scenarios 7, 15, 16 and 17, the gate, the limits and probation.

use std::collections::BTreeMap;
use std::sync::Mutex;

use chrono::TimeDelta;
use rustykrab_core::outcome::OutcomeTally;
use rustykrab_core::proposal::{Criterion, METRICS, UNKNOWN_ERROR_RATE};
use rustykrab_core::work::{
    required_review_tier, Edge, EdgeKind, ErrorClass, ErrorSubclass, EventKind, PlanAccepted,
    PlanRejected, ReviewTier, Rung, RungEvent, Status, WorkError, WorkEvent, WorkFacets,
    WorkItemId, WorkKind, WorkerKind,
};

use super::*;

// ── fakes ───────────────────────────────────────────────────────────────

#[derive(Default)]
struct FakeWork(Mutex<WorkRecords>);

#[async_trait]
impl WorkRecordSource for FakeWork {
    async fn records(&self, _since: DateTime<Utc>) -> Result<WorkRecords> {
        Ok(self.0.lock().unwrap().clone())
    }
}

#[derive(Default)]
struct FakeOutcomes {
    ground_truth: Vec<(String, OutcomeTally)>,
    all: Vec<(String, OutcomeTally)>,
}

#[async_trait]
impl OutcomeSource for FakeOutcomes {
    async fn tallies(
        &self,
        _kind: AttributionKind,
        ground_truth_only: bool,
    ) -> Result<Vec<(String, OutcomeTally)>> {
        Ok(if ground_truth_only {
            self.ground_truth.clone()
        } else {
            self.all.clone()
        })
    }

    async fn total_records(&self) -> Result<u32> {
        Ok(0)
    }

    async fn verdict_totals(&self, _ground_truth_only: bool) -> Result<OutcomeTally> {
        Ok(OutcomeTally::default())
    }

    async fn cite(
        &self,
        _kind: AttributionKind,
        _ground_truth_only: bool,
        _limit: usize,
    ) -> Result<BTreeMap<String, (Vec<String>, Vec<String>)>> {
        Ok(self
            .ground_truth
            .iter()
            .map(|(s, _)| (s.clone(), (vec![format!("rec-{s}-1")], Vec::new())))
            .collect())
    }
}

/// Files everything, refusing a protected subject below the highest tier
/// as the controller's validator does.
#[derive(Default)]
struct FakeFiler {
    drafts: Mutex<Vec<WorkItemDraft>>,
    internal: Mutex<Vec<WorkError>>,
}

impl FakeFiler {
    fn drafts(&self) -> Vec<WorkItemDraft> {
        self.drafts.lock().unwrap().clone()
    }
}

fn accepted(id: String) -> PlanOutcome {
    PlanOutcome::Accepted(PlanAccepted {
        root: id,
        ids: Default::default(),
        held: vec![],
        policy: None,
        warnings: vec![],
    })
}

#[async_trait]
impl ProposalFiler for FakeFiler {
    async fn file(&self, draft: WorkItemDraft) -> Result<PlanOutcome> {
        if draft.review_tier.unwrap_or_default() < required_review_tier(draft.subject.as_deref()) {
            return Ok(PlanOutcome::Rejected(PlanRejected { failed: vec![] }));
        }
        let mut drafts = self.drafts.lock().unwrap();
        drafts.push(draft);
        Ok(accepted(format!("proposal-{}", drafts.len())))
    }

    async fn file_internal(
        &self,
        error: &WorkError,
        _evidence: Vec<ArtifactRef>,
    ) -> Result<PlanOutcome> {
        let mut internal = self.internal.lock().unwrap();
        internal.push(error.clone());
        Ok(accepted(format!("internal-{}", internal.len())))
    }
}

#[derive(Default)]
struct FakeLedger {
    passes: Mutex<Vec<Vec<MetricValue>>>,
    rows: Mutex<Vec<ProposalRow>>,
}

#[async_trait]
impl EvaluationLedger for FakeLedger {
    async fn latest_metrics(&self) -> Result<Vec<MetricValue>> {
        Ok(self
            .passes
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default())
    }

    async fn record_metrics(&self, _pass: &str, values: &[MetricValue]) -> Result<()> {
        self.passes.lock().unwrap().push(values.to_vec());
        Ok(())
    }

    async fn proposals(&self) -> Result<Vec<ProposalRow>> {
        Ok(self.rows.lock().unwrap().clone())
    }

    async fn record_proposal(&self, row: &ProposalRow, _e: &[ProposalEvidence]) -> Result<()> {
        self.rows.lock().unwrap().push(row.clone());
        Ok(())
    }

    async fn record_review(
        &self,
        item: &str,
        review: ReviewState,
        decided_by: &str,
        decided_at: DateTime<Utc>,
        code_item: Option<&str>,
        baseline: Option<f64>,
    ) -> Result<()> {
        for r in self
            .rows
            .lock()
            .unwrap()
            .iter_mut()
            .filter(|r| r.item == item)
        {
            r.review = review;
            r.decided_by = Some(decided_by.to_string());
            r.decided_at = Some(decided_at);
            r.code_item = code_item.map(str::to_string);
            r.baseline = baseline;
        }
        Ok(())
    }

    async fn record_outcome(
        &self,
        item: &str,
        outcome: ProposalOutcome,
        observed: Option<f64>,
        at: DateTime<Utc>,
    ) -> Result<()> {
        for r in self
            .rows
            .lock()
            .unwrap()
            .iter_mut()
            .filter(|r| r.item == item)
        {
            r.outcome = outcome;
            r.observed = observed;
            r.outcome_at = Some(at);
        }
        Ok(())
    }
}

struct Rig {
    work: Arc<FakeWork>,
    filer: Arc<FakeFiler>,
    ledger: Arc<FakeLedger>,
    eval: Evaluation,
}

fn rig(
    outcomes: FakeOutcomes,
    questions: Vec<SurfacedQuestion>,
    routing: Vec<RoutingEntry>,
) -> Rig {
    let work = Arc::new(FakeWork::default());
    let filer = Arc::new(FakeFiler::default());
    let ledger = Arc::new(FakeLedger::default());
    let eval = Evaluation {
        work: work.clone(),
        outcomes: Arc::new(outcomes),
        questions: Arc::new(StaticQuestions(questions)),
        routing: Arc::new(StaticRouting(routing)),
        filer: filer.clone(),
        ledger: ledger.clone(),
        config: EvaluationConfig::default(),
    };
    Rig {
        work,
        filer,
        ledger,
        eval,
    }
}

fn now() -> DateTime<Utc> {
    Utc::now()
}

fn item(id: &str, kind: WorkKind, status: Status) -> ItemRecord {
    ItemRecord {
        id: id.into(),
        kind,
        status,
        title: format!("title of {id}"),
        parent: None,
        created_at: now() - TimeDelta::hours(1),
        closed_at: status.is_closed().then(|| now() - TimeDelta::minutes(30)),
        artifact_refs: vec![],
        archived: false,
    }
}

fn error(class: ErrorClass, subclass: ErrorSubclass, fp: &str) -> WorkError {
    WorkError {
        class,
        subclass,
        fingerprint: fp.into(),
        detail: format!("detail {fp}"),
        artifact_refs: vec![],
        observed_by: "tool_result".into(),
    }
}

fn rung_event(item: &str, rung: Rung, err: Option<WorkError>, at: DateTime<Utc>) -> WorkEvent {
    let r = RungEvent {
        rung,
        at,
        error: err,
        outcome: "tried".into(),
    };
    WorkEvent {
        item: item.into(),
        at,
        kind: EventKind::Rung,
        from: None,
        to: None,
        actor: "controller".into(),
        reason: Some(serde_json::to_string(&r).unwrap()),
        upstream: None,
        origin: None,
        evidence_ref: None,
    }
}

fn tally(helpful: u32, harmful: u32) -> OutcomeTally {
    OutcomeTally {
        helpful,
        harmful,
        ambiguous: 0,
    }
}

// ── scenarios ───────────────────────────────────────────────────────────

/// Scenario 7's dreaming half: a verifiable signal files exactly one
/// proposal; an implicit one files none and is reported as gated.
#[tokio::test]
async fn a_verifiable_signal_files_one_proposal_and_an_implicit_one_none() {
    let outcomes = FakeOutcomes {
        ground_truth: vec![("verifiable-skill".into(), tally(0, 6))],
        all: vec![
            ("verifiable-skill".into(), tally(0, 6)),
            ("implicit-skill".into(), tally(0, 6)),
        ],
    };
    let r = rig(outcomes, vec![], vec![]);
    let report = r.eval.run(now()).await.unwrap();
    let drafts = r.filer.drafts();
    assert_eq!(drafts.len(), 1, "{drafts:?}");
    let d = &drafts[0];
    assert_eq!(d.kind, Some(WorkKind::Proposal));
    assert_eq!(d.subject.as_deref(), Some("skill:verifiable-skill"));
    assert!(d.title.contains("verifiable-skill"));
    for text in [&d.title, &d.objective, &d.done_when] {
        assert!(!text.contains("implicit-skill"));
    }
    // Section 10's shape is all there.
    for part in [
        "Observed:",
        "Expectation served:",
        "Would change:",
        "Metric:",
        "Evidence:",
        "Risk:",
        "Rollback:",
        "Falsified by:",
    ] {
        assert!(
            d.objective.contains(part),
            "missing {part}: {}",
            d.objective
        );
    }
    assert!(d
        .artifact_refs
        .iter()
        .any(|a| a.kind == "outcome_record" && a.value == "rec-verifiable-skill-1"));
    assert!(report
        .skipped
        .iter()
        .any(|s| s.subject == "skill:implicit-skill" && s.why.contains("ground-truth gate")));
    let rows = r.ledger.rows.lock().unwrap().clone();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].criterion, Criterion::SkillOutcome);
    assert_eq!(rows[0].metric, "skill:verifiable-skill");
    // The next pass files nothing new: one live proposal per subject.
    r.eval.run(now()).await.unwrap();
    assert_eq!(r.filer.drafts().len(), 1);
}

#[tokio::test]
async fn every_metric_has_a_finite_value_even_with_nothing_to_measure() {
    let r = rig(FakeOutcomes::default(), vec![], vec![]);
    let report = r.eval.run(now()).await.unwrap();
    assert_eq!(report.metrics.len(), METRICS.len());
    assert!(report.metrics.len() >= 8);
    for m in &report.metrics {
        assert!(m.value.is_finite(), "{}", m.name);
        let json = serde_json::to_value(m).unwrap();
        assert!(!json["value"].is_null(), "{}", m.name);
    }
    assert!(
        report.regressions.is_empty(),
        "no previous pass, no regression"
    );
}

/// Scenario 16's dreaming half: new unknown errors since the previous
/// pass regress the unknown-error rate and file a proposal naming it.
#[tokio::test]
async fn new_unknown_errors_regress_the_metric_and_a_proposal_names_it() {
    let r = rig(FakeOutcomes::default(), vec![], vec![]);
    let t0 = now() - TimeDelta::minutes(20);
    {
        let mut w = r.work.0.lock().unwrap();
        w.items.push(item("a", WorkKind::Personal, Status::Failed));
        w.items.push(item("b", WorkKind::Personal, Status::Done));
        w.events.push(rung_event(
            "a",
            Rung::Improve,
            Some(error(
                ErrorClass::Unknown,
                ErrorSubclass::Unclassified,
                "fp1",
            )),
            t0,
        ));
        w.events.push(rung_event(
            "b",
            Rung::Retry,
            Some(error(ErrorClass::Tool, ErrorSubclass::Timeout, "fp2")),
            t0,
        ));
    }
    let first = r.eval.run(now() - TimeDelta::minutes(10)).await.unwrap();
    assert!(first.regressions.is_empty());
    let unknown = first
        .metrics
        .iter()
        .find(|m| m.name == UNKNOWN_ERROR_RATE)
        .unwrap();
    assert_eq!(unknown.value, 0.5);
    // The unknown fingerprint had no internal item: the pass filed one,
    // which the controller then holds like any other item.
    assert_eq!(r.filer.internal.lock().unwrap().len(), 1);
    {
        let mut w = r.work.0.lock().unwrap();
        let mut internal = item("internal-1", WorkKind::Internal, Status::Queued);
        internal.title = "Classify unknown failure fp1".into();
        w.items.push(internal);
        for id in ["c", "d"] {
            w.items.push(item(id, WorkKind::Personal, Status::Failed));
            w.events.push(rung_event(
                id,
                Rung::Improve,
                Some(error(
                    ErrorClass::Unknown,
                    ErrorSubclass::Unclassified,
                    "fp1",
                )),
                now() - TimeDelta::minutes(5),
            ));
        }
    }
    let second = r.eval.run(now()).await.unwrap();
    assert!(second.regressions.contains(&UNKNOWN_ERROR_RATE.to_string()));
    let named: Vec<_> = r
        .filer
        .drafts()
        .into_iter()
        .filter(|d| d.title.contains(UNKNOWN_ERROR_RATE))
        .collect();
    assert_eq!(named.len(), 1, "{:?}", r.filer.drafts());
    assert_eq!(
        named[0].subject.as_deref(),
        Some("expectation:unknown_error_rate")
    );
    // Not a second time.
    assert_eq!(r.filer.internal.lock().unwrap().len(), 1);
}

/// Scenario 15's dreaming half.
#[tokio::test]
async fn an_answer_that_was_the_recorded_default_is_an_avoidable_escalation() {
    let q = |id: &str, answer: &str, via: Option<&str>| SurfacedQuestion {
        id: id.into(),
        item: format!("item-{id}"),
        class: "blocking_now".into(),
        options: vec!["9am".into(), "10am".into()],
        recorded_default: Some("9am".into()),
        delivered_via: via.map(str::to_string),
        answer: Some(answer.into()),
        answered_at: Some(now()),
    };
    let questions = vec![
        q("q1", " 9AM ", Some("telegram")),
        q("q2", "10am", Some("telegram")),
        q("q3", "9am", None),
    ];
    let r = rig(FakeOutcomes::default(), questions, vec![]);
    let report = r.eval.run(now()).await.unwrap();
    let avoidable = report
        .metrics
        .iter()
        .find(|m| m.name == "avoidable_escalations")
        .unwrap();
    assert_eq!(avoidable.value, 1.0);
    let drafts = r.filer.drafts();
    let d = drafts
        .iter()
        .find(|d| d.title.to_lowercase().contains("avoidable"))
        .expect("an avoidable-escalation proposal");
    assert!(d.artifact_refs.iter().any(|a| a.value == "item-q1"));
    assert!(!d.artifact_refs.iter().any(|a| a.value == "item-q2"));
    // It touches standing judgment, which is policy: the highest tier.
    assert_eq!(d.review_tier, Some(ReviewTier::Highest));
}

/// Scenario 17's dreaming half: a routing proposal cites both records;
/// nothing changes the default on its own.
#[tokio::test]
async fn a_routing_proposal_cites_both_workers_records() {
    let entry = |worker: &str, kind, tier, verified, missed, default| RoutingEntry {
        worker: worker.into(),
        worker_kind: kind,
        cost_tier: tier,
        class: "code:small".into(),
        verified_done: verified,
        claimed_not_verified: missed,
        escaped_defects: 0,
        review_rejections: 0,
        repairs: 0,
        cost: 0.0,
        probation: false,
        default_for_class: default,
        default_tier: Some(0),
        items: vec![format!("item-{worker}")],
    };
    let routing = vec![
        entry("pinch", WorkerKind::Local, 0, 1, 5, true),
        entry("claws", WorkerKind::ClaudeCode, 2, 6, 0, false),
    ];
    let r = rig(FakeOutcomes::default(), vec![], routing);
    r.eval.run(now()).await.unwrap();
    let d = r
        .filer
        .drafts()
        .into_iter()
        .find(|d| d.subject.as_deref() == Some("routing:code:small"))
        .expect("a routing proposal");
    let cited: Vec<_> = d.artifact_refs.iter().map(|a| a.value.as_str()).collect();
    assert!(cited.contains(&"pinch:code:small") && cited.contains(&"claws:code:small"));
    assert!(d.title.contains("from pinch to claws"));
    // The items behind both records, and the move itself, typed.
    assert!(cited.contains(&"item-pinch") && cited.contains(&"item-claws"));
    assert!(
        d.artifact_refs
            .iter()
            .any(|a| a.kind == crate::evaluate::criteria::ROUTING_DEFAULT
                && a.value == "2 code:small")
    );
}

#[tokio::test]
async fn the_daily_cap_and_the_decline_cooldown_hold() {
    let skills: Vec<(String, OutcomeTally)> =
        (0..15).map(|i| (format!("s{i:02}"), tally(0, 6))).collect();
    let r = rig(
        FakeOutcomes {
            ground_truth: skills.clone(),
            all: skills,
        },
        vec![],
        vec![],
    );
    let report = r.eval.run(now()).await.unwrap();
    assert_eq!(r.filer.drafts().len(), 10);
    assert!(report
        .skipped
        .iter()
        .any(|s| s.why.starts_with("rate limit")));
    // A declined subject stays closed for the cool-down.
    {
        let mut rows = r.ledger.rows.lock().unwrap();
        rows[0].review = ReviewState::Declined;
        rows[0].decided_at = Some(now());
        for row in rows.iter_mut() {
            row.created_at = now() - TimeDelta::days(2);
        }
    }
    let declined_subject = r.ledger.rows.lock().unwrap()[0].subject.clone();
    r.eval.run(now()).await.unwrap();
    let refiled = r
        .filer
        .drafts()
        .iter()
        .filter(|d| d.subject.as_deref() == Some(declined_subject.as_str()))
        .count();
    assert_eq!(refiled, 1, "declined inside the cool-down is not re-filed");
}

#[tokio::test]
async fn probation_records_whether_the_metric_moved() {
    let r = rig(
        FakeOutcomes {
            ground_truth: vec![("cal".into(), tally(1, 5))],
            all: vec![("cal".into(), tally(1, 5))],
        },
        vec![],
        vec![],
    );
    r.eval.run(now()).await.unwrap();
    let proposal: WorkItemId = r.ledger.rows.lock().unwrap()[0].item.clone();
    // Accepted: the proposal closed done and a code item names it.
    {
        let mut w = r.work.0.lock().unwrap();
        w.items
            .push(item(&proposal, WorkKind::Proposal, Status::Done));
        let mut code = item("code-1", WorkKind::Code, Status::Running);
        code.closed_at = None;
        w.items.push(code);
        w.edges.push(Edge {
            item: "code-1".into(),
            depends_on: proposal.clone(),
            kind: EdgeKind::DiscoveredFrom,
        });
    }
    r.eval.run(now()).await.unwrap();
    let row = r.ledger.rows.lock().unwrap()[0].clone();
    assert_eq!(row.review, ReviewState::Accepted);
    assert_eq!(row.code_item.as_deref(), Some("code-1"));
    let baseline = row.baseline.expect("a baseline at acceptance");
    assert!((baseline - 1.0 / 6.0).abs() < 1e-9);
    // The change lands, probation passes, and the skill did not improve.
    {
        let mut w = r.work.0.lock().unwrap();
        let code = w.items.iter_mut().find(|i| i.id == "code-1").unwrap();
        code.status = Status::Done;
        code.closed_at = Some(now() - TimeDelta::days(8));
    }
    let report = r.eval.run(now()).await.unwrap();
    assert_eq!(report.probation.len(), 1);
    assert_eq!(report.probation[0].outcome, ProposalOutcome::NotMoved);
    assert_eq!(
        r.ledger.rows.lock().unwrap()[0].outcome,
        ProposalOutcome::NotMoved
    );
    let moved = report
        .metrics
        .iter()
        .find(|m| m.name == "proposals_moved_metric_rate")
        .unwrap();
    assert_eq!(moved.sample, 0, "the pass measured before recording");
    assert_eq!(
        file::verdict("skill:cal", 0.2, 0.9, 0.05),
        ProposalOutcome::Moved
    );
    assert_eq!(
        file::verdict(UNKNOWN_ERROR_RATE, 0.2, 0.5, 0.05),
        ProposalOutcome::Regressed
    );
}

#[tokio::test]
async fn an_unknown_error_with_an_internal_item_files_nothing_more() {
    let r = rig(FakeOutcomes::default(), vec![], vec![]);
    {
        let mut w = r.work.0.lock().unwrap();
        w.items.push(item("a", WorkKind::Personal, Status::Failed));
        let mut internal = item("i", WorkKind::Internal, Status::Queued);
        internal.title = "Classify unknown failure fp9".into();
        w.items.push(internal);
        w.events.push(rung_event(
            "a",
            Rung::Improve,
            Some(error(
                ErrorClass::Unknown,
                ErrorSubclass::Unclassified,
                "fp9",
            )),
            now(),
        ));
        w.facets.insert(
            "i".into(),
            WorkFacets {
                ..WorkFacets::default()
            },
        );
    }
    r.eval.run(now()).await.unwrap();
    assert!(r.filer.internal.lock().unwrap().is_empty());
}
