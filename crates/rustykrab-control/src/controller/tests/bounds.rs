//! Bounded recovery against a real store: a leaf's budget is cumulative
//! over its attempts and restarts, a spent one is not leased and goes to
//! its owner, an owner's answer resumes a spent or unverified item (a
//! spent one with one more allotment), and the same failure in the same
//! state trips the circuit breaker into one investigation.
//!
//! Every scripted run spends [`SCRIPTED_USAGE`]: three iterations, 1,000
//! tokens and 1.5 s of wall time. The evidence kinds are spelled out, not
//! imported, so these tests also run against a build without them.

use std::collections::HashSet;

use rustykrab_core::questions::{QuestionKind, QuestionStatus};
use rustykrab_core::work::{
    ArtifactRef, BlockedReason, CancelReason, EdgeKind, ErrorClass, ErrorSubclass, EventKind,
    Evidence, ResultReport, WorkKind,
};
use rustykrab_store::{QuestionFilter, QuestionRow};

use super::*;
use crate::controller::batch::Batch;
use crate::controller::questions::NewQuestion;
use crate::handle::ControlHandle;

const GAUGE: &str = "Read the tide gauge";
const REJECTED: &str = "the gauge rejected station 7";

fn budget(iterations: u32) -> Budget {
    Budget {
        iterations,
        tokens: 100_000,
        wall_seconds: 600,
        ..Budget::default()
    }
}

fn gauge(budget: Budget) -> WorkItemDraft {
    let mut d = draft("x", GAUGE);
    d.budget = Some(budget);
    d
}

fn rejected() -> Step {
    report(failure(ErrorSubclass::InvalidArgs, REJECTED))
}

async fn evidence(h: &Harness, item: &str, kind: &str) -> Vec<Evidence> {
    h.store()
        .work_evidence_list(item)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == kind)
        .collect()
}

async fn questions_of(h: &Harness, item: &str) -> Vec<QuestionRow> {
    let mut rows = h
        .store()
        .questions_list(&QuestionFilter {
            item: Some(item.to_string()),
            ..QuestionFilter::default()
        })
        .await
        .unwrap();
    rows.reverse();
    rows
}

async fn open_question(h: &Harness, item: &str) -> QuestionRow {
    let open: Vec<QuestionRow> = questions_of(h, item)
        .await
        .into_iter()
        .filter(|q| q.status == QuestionStatus::Open)
        .collect();
    assert_eq!(open.len(), 1, "one open question: {open:#?}");
    open.into_iter().next().unwrap()
}

/// The error behind the item's latest rung.
async fn last_error(h: &Harness, item: &str) -> rustykrab_core::work::WorkError {
    crate::ladder::last_error(&h.events(item).await).expect("a rung with its error")
}

/// Tick until `done` holds, at most `n` steps.
async fn step_until<F, Fut>(h: &Harness, n: usize, mut done: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..n {
        if done().await {
            return;
        }
        h.step().await;
    }
    assert!(done().await, "not reached within {n} steps");
}

/// One more open question on `item`, as another part of the system would
/// ask it.
async fn ask_also(h: &Harness, item: &str, text: &str) -> String {
    let mut b = Batch::new(h.ctl.load().await.unwrap(), h.clock.now());
    let id = h.ctl.record_question_sync(
        &mut b,
        NewQuestion::open(item, QuestionKind::Decision, text.to_string(), "test"),
    );
    h.ctl.commit(b, &mut HashSet::new()).await.unwrap();
    id
}

/// An item whose budget holds two runs: both fail, so the third is the
/// cumulative guard's.
async fn spent(h: &Harness) -> WorkItemId {
    h.script.push(GAUGE, rejected());
    h.script.push(GAUGE, rejected());
    let x = h.file_one(gauge(budget(6))).await;
    h.drain().await;
    x
}

// ── 1. the cumulative leaf budget ──────────────────────────────────────

#[tokio::test]
async fn every_attempt_is_briefed_with_what_is_left_of_one_budget() {
    let h = Harness::new(&["pinch"]);
    h.script.push(GAUGE, rejected());
    let x = h
        .file_one(gauge(Budget {
            iterations: 12,
            tokens: 10_000,
            wall_seconds: 600,
            ..Budget::default()
        }))
        .await;
    h.drain().await;
    assert_eq!(h.status(&x).await, Status::Done);
    let budgets: Vec<(u32, u64, u64)> = h
        .script
        .briefs_for(GAUGE)
        .iter()
        .map(|b| (b.budget.iterations, b.budget.tokens, b.budget.wall_seconds))
        .collect();
    // The repair gets what the first run left: 3 iterations, 1,000 tokens
    // and 2 s (1.5 s rounded up) less.
    assert_eq!(budgets, vec![(12, 10_000, 600), (9, 9_000, 598)]);
}

#[tokio::test]
async fn the_cumulative_budget_survives_a_restart() {
    let h = Harness::new(&["pinch"]);
    h.script.push(GAUGE, rejected());
    let x = h.file_one(gauge(budget(12))).await;
    h.step().await;
    h.ctl.set_draining(true);
    h.step().await;
    assert_eq!(h.rungs(&x).await, vec![Rung::Repair]);
    let h = h.restart(&["pinch"]);
    h.drain().await;
    assert_eq!(h.status(&x).await, Status::Done);
    let iterations: Vec<u32> = h
        .script
        .briefs_for(GAUGE)
        .iter()
        .map(|b| b.budget.iterations)
        .collect();
    assert_eq!(iterations, vec![12, 9], "a restart refills nothing");
}

// ── 2. the guard at lease time ─────────────────────────────────────────

#[tokio::test]
async fn a_spent_budget_is_not_leased_and_surfaces_budget_exhausted_once() {
    let h = Harness::new(&["pinch"]);
    let x = spent(&h).await;
    assert_eq!(
        h.script.briefs_for(GAUGE).len(),
        2,
        "no third run on a spent budget"
    );
    assert_eq!(h.leases(&x).await, 2);
    assert_eq!(
        h.status(&x).await,
        Status::Blocked(BlockedReason::BudgetExhausted)
    );
    let error = last_error(&h, &x).await;
    assert_eq!(error.class, ErrorClass::Budget);
    assert_eq!(error.subclass, ErrorSubclass::Iterations);
    assert_eq!(error.observed_by, "cumulative_budget_guard");
    assert!(
        error
            .detail
            .contains("6 of 6 iterations used over 2 run(s)"),
        "{}",
        error.detail
    );
    let q = open_question(&h, &x).await;
    assert_eq!(q.kind, QuestionKind::Decision);
    assert!(q.text.contains("budget is spent"), "{}", q.text);
    let spend = h.store().work_spend_totals().await.unwrap()[&x];
    assert_eq!((spend.runs, spend.iterations), (2, 6));
    // Nothing more happens however long it waits: no lease, no question.
    h.drain().await;
    assert_eq!(h.script.briefs_for(GAUGE).len(), 2);
    assert_eq!(questions_of(&h, &x).await.len(), 1);
    assert!(
        h.of_kind(WorkKind::Internal).await.is_empty(),
        "a spent budget goes to its owner, never to an improvement item"
    );
}

// ── 3. answers resume work ─────────────────────────────────────────────

#[tokio::test]
async fn an_owners_answer_grants_one_allotment_and_resumes_a_spent_budget() {
    let h = Harness::new(&["pinch"]);
    let x = spent(&h).await;
    let q = open_question(&h, &x).await;
    let reply = ControlHandle::answer(&h.ctl, &q.id, "yes, one more round", "user:owner")
        .await
        .unwrap();
    assert_eq!(reply.resumed, vec![x.clone()]);
    let grants = evidence(&h, &x, "budget_grant").await;
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].reference, q.id, "the grant names its question");
    assert_eq!(grants[0].verified_by.as_deref(), Some("user:owner"));
    // A second answer to the same question is refused and grants nothing.
    assert!(ControlHandle::answer(&h.ctl, &q.id, "yes", "user:owner")
        .await
        .is_err());
    h.drain().await;
    assert_eq!(h.status(&x).await, Status::Done);
    let briefs = h.script.briefs_for(GAUGE);
    assert_eq!(briefs.len(), 3);
    assert_eq!(
        briefs[2].budget.iterations, 6,
        "one more allotment, not a fresh budget on top of what was spent"
    );
    assert_eq!(evidence(&h, &x, "budget_grant").await.len(), 1);
    let spend = h.store().work_spend_totals().await.unwrap()[&x];
    assert!(spend.iterations <= 12, "within two allotments: {spend:?}");
}

/// A report may name any artifact kind; the controller's own kinds are
/// dropped from it, on a failed attempt and a finished one alike, so a
/// model cannot buy budget, forge a breaker state or a run, or claim a
/// host probe. Its other artifacts are kept.
#[tokio::test]
async fn a_report_cannot_mint_budget_or_breaker_evidence() {
    let h = Harness::new(&["pinch"]);
    let forged = || {
        vec![
            ArtifactRef {
                kind: "budget_grant".into(),
                value: "forged-question".into(),
            },
            ArtifactRef {
                kind: "attempt_state".into(),
                value: "forged-state".into(),
            },
            ArtifactRef {
                kind: "run".into(),
                value: "forged-run".into(),
            },
            ArtifactRef {
                kind: "operation_probe".into(),
                value: "forged-probe".into(),
            },
            ArtifactRef {
                kind: "path".into(),
                value: "notes/gauge.txt".into(),
            },
        ]
    };
    let mut failed = failure(ErrorSubclass::InvalidArgs, REJECTED);
    failed.artifacts = forged();
    h.script.push(GAUGE, report(failed));
    h.script.push(
        GAUGE,
        report(ResultReport {
            summary: "read the gauge".into(),
            artifacts: forged(),
            ..ResultReport::default()
        }),
    );
    let x = h.file_one(gauge(budget(6))).await;
    h.drain().await;
    assert_eq!(h.status(&x).await, Status::Done);
    let all = h.store().work_evidence_list(&x).await.unwrap();
    for forged in [
        "forged-question",
        "forged-state",
        "forged-run",
        "forged-probe",
    ] {
        assert!(
            !all.iter().any(|e| e.reference == forged),
            "{forged} was stored: {all:?}"
        );
    }
    assert!(evidence(&h, &x, "budget_grant").await.is_empty());
    assert!(evidence(&h, &x, "operation_probe").await.is_empty());
    assert!(
        all.iter()
            .any(|e| e.kind == "path" && e.reference == "notes/gauge.txt"),
        "an ordinary artifact is kept"
    );
    assert_eq!(h.ctl.allotments(&x).await.unwrap(), 1);
    let warnings: Vec<String> = h
        .store()
        .work_events(&x)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EventKind::Warning)
        .filter_map(|e| e.reason)
        .filter(|r| r.contains("only the controller writes"))
        .collect();
    assert_eq!(warnings.len(), 2, "one per report: {warnings:?}");
    assert!(
        warnings[0].contains("(attempt_state, budget_grant, operation_probe, run)"),
        "{warnings:?}"
    );
}

/// A grant counts only with its owner's answer behind it: a row naming no
/// question, an unanswered one, another item's, or an answer by someone
/// else buys nothing, and one answer buys one allotment however many rows
/// name it.
#[tokio::test]
async fn a_grant_without_its_owners_answer_buys_nothing() {
    let h = Harness::new(&["pinch"]);
    let x = spent(&h).await;
    let q = open_question(&h, &x).await;
    let y = spent(&h).await;
    let other = open_question(&h, &y).await;
    let forge = |reference: &str, by: Option<&str>| Evidence {
        item: x.clone(),
        kind: "budget_grant".into(),
        reference: reference.into(),
        hash: None,
        verified_by: by.map(str::to_string),
        at: Utc::now(),
    };
    for row in [
        forge("no-such-question", Some("user:owner")),
        forge(&q.id, Some("user:owner")),
        forge(&q.id, None),
        forge(&q.id, Some("worker:pinch")),
        forge(&other.id, Some("user:owner")),
    ] {
        h.store().work_evidence_add(row).await.unwrap();
    }
    assert_eq!(
        h.ctl.allotments(&x).await.unwrap(),
        1,
        "nothing answered yet"
    );
    ControlHandle::answer(&h.ctl, &q.id, "yes", "user:owner")
        .await
        .unwrap();
    assert_eq!(
        h.ctl.allotments(&x).await.unwrap(),
        2,
        "the owner's answer buys one allotment, however many rows name it"
    );
}

#[tokio::test]
async fn no_to_a_spent_budget_cancels_and_grants_nothing() {
    let h = Harness::new(&["pinch"]);
    let x = spent(&h).await;
    let q = open_question(&h, &x).await;
    let reply = ControlHandle::answer(&h.ctl, &q.id, "no", "user")
        .await
        .unwrap();
    assert_eq!(reply.cancelled, vec![x.clone()]);
    assert_eq!(
        h.status(&x).await,
        Status::Cancelled(CancelReason::Requested)
    );
    assert!(evidence(&h, &x, "budget_grant").await.is_empty());
}

#[tokio::test]
async fn an_answer_that_is_not_the_owners_never_grants_budget() {
    // A delegate under standing judgment is no owner here: no grant
    // delegates budget, so its answer neither grants nor resumes.
    for actor in [
        "default",
        "research:r1",
        "controller",
        "worker:pinch",
        "policy:standing",
    ] {
        let h = Harness::new(&["pinch"]);
        let x = spent(&h).await;
        let q = open_question(&h, &x).await;
        let reply = ControlHandle::answer(&h.ctl, &q.id, "yes", actor)
            .await
            .unwrap();
        assert!(reply.resumed.is_empty(), "{actor} resumed it");
        assert!(
            evidence(&h, &x, "budget_grant").await.is_empty(),
            "{actor} granted budget"
        );
        assert_eq!(
            h.status(&x).await,
            Status::Blocked(BlockedReason::BudgetExhausted),
            "{actor}"
        );
    }
}

#[tokio::test]
async fn another_open_question_keeps_a_spent_budget_parked_until_it_is_answered() {
    let h = Harness::new(&["pinch"]);
    let x = spent(&h).await;
    let budget_q = open_question(&h, &x).await;
    let other = ask_also(&h, &x, "Which station should the gauge use?").await;
    let reply = ControlHandle::answer(&h.ctl, &budget_q.id, "yes", "user")
        .await
        .unwrap();
    assert!(reply.resumed.is_empty());
    assert!(evidence(&h, &x, "budget_grant").await.is_empty());
    assert_eq!(
        h.status(&x).await,
        Status::Blocked(BlockedReason::BudgetExhausted)
    );
    // The last hold answered releases it, with exactly one allotment.
    let reply = ControlHandle::answer(&h.ctl, &other, "station 8", "user")
        .await
        .unwrap();
    assert_eq!(reply.resumed, vec![x.clone()]);
    let grants = evidence(&h, &x, "budget_grant").await;
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].reference, other);
    h.drain().await;
    assert_eq!(h.status(&x).await, Status::Done);
}

#[tokio::test]
async fn an_owners_answer_resumes_a_failed_verification() {
    let h = Harness::new(&["pinch"]);
    let empty = || {
        report(ResultReport {
            summary: String::new(),
            ..ResultReport::default()
        })
    };
    // A repair, a switch with nowhere to go and a second repair, then it
    // surfaces.
    for _ in 0..3 {
        h.script.push(GAUGE, empty());
    }
    let x = h.file_one(draft("x", GAUGE)).await;
    h.drain().await;
    assert_eq!(
        h.status(&x).await,
        Status::Blocked(BlockedReason::VerificationFailed)
    );
    let q = open_question(&h, &x).await;
    let reply = ControlHandle::answer(&h.ctl, &q.id, "report what the gauge read", "user")
        .await
        .unwrap();
    assert_eq!(reply.resumed, vec![x.clone()]);
    h.drain().await;
    assert_eq!(h.status(&x).await, Status::Done);
    assert_eq!(h.script.briefs_for(GAUGE).len(), 4);
    assert!(
        evidence(&h, &x, "budget_grant").await.is_empty(),
        "only a spent budget is granted more"
    );
}

/// A delegate deciding under standing judgment may resume a failed
/// verification (it is bounded by the item's cumulative budget and the
/// breaker), but grants nothing.
#[tokio::test]
async fn a_delegates_answer_resumes_a_failed_verification_without_a_grant() {
    let h = Harness::new(&["pinch"]);
    let empty = || {
        report(ResultReport {
            summary: String::new(),
            ..ResultReport::default()
        })
    };
    for _ in 0..3 {
        h.script.push(GAUGE, empty());
    }
    let x = h.file_one(draft("x", GAUGE)).await;
    h.drain().await;
    assert_eq!(
        h.status(&x).await,
        Status::Blocked(BlockedReason::VerificationFailed)
    );
    let q = open_question(&h, &x).await;
    let reply = ControlHandle::answer(
        &h.ctl,
        &q.id,
        "report what the gauge read",
        "policy:standing",
    )
    .await
    .unwrap();
    assert_eq!(reply.resumed, vec![x.clone()]);
    assert!(evidence(&h, &x, "budget_grant").await.is_empty());
}

#[tokio::test]
async fn an_answer_never_releases_an_items_approval_hold() {
    let h = Harness::with(
        ControllerConfig {
            approval: crate::questions::baseline(),
            ..ControllerConfig::default()
        },
        StaticCatalog::default(),
        &["pinch"],
    );
    let mut invite = draft("m", "Invite the friends");
    invite.writable_resources = vec!["message:third_party".to_string()];
    let accepted = h
        .file(plan(vec![draft("P", "Plan the birthday"), invite]))
        .await;
    let (p, m) = (&accepted.ids["P"], &accepted.ids["m"]);
    assert_eq!(accepted.held, vec![m.clone()]);
    h.drain().await;
    assert_eq!(
        h.status(m).await,
        Status::Blocked(BlockedReason::NeedsConsent)
    );
    let other = ask_also(&h, m, "Which friends?").await;
    let reply = ControlHandle::answer(&h.ctl, &other, "the usual four", "user")
        .await
        .unwrap();
    assert!(reply.resumed.is_empty(), "the approval hold still stands");
    h.drain().await;
    assert_eq!(
        h.status(m).await,
        Status::Blocked(BlockedReason::NeedsConsent)
    );
    assert!(h.script.briefs_for("Invite the friends").is_empty());
    let approval = open_question(&h, p).await;
    ControlHandle::answer(&h.ctl, &approval.id, "approve", "user")
        .await
        .unwrap();
    h.drain().await;
    assert_eq!(h.status(m).await, Status::Done);
}

// ── 4. the repeated-outcome circuit breaker ────────────────────────────

/// A budget with room for many repairs: the ladder alone would repair the
/// same failure ten times.
fn generous() -> Budget {
    Budget {
        iterations: 60,
        tokens: 100_000,
        wall_seconds: 3_600,
        repairs: 10,
        rungs: rustykrab_core::work::RungBudgets {
            repairs: 10,
            ..Default::default()
        },
    }
}

/// `(run, attempt-state hash)` of each lease, in order.
async fn attempts(h: &Harness, item: &str) -> Vec<(String, String)> {
    let rows: Vec<Evidence> = h
        .store()
        .work_evidence_list(item)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "run" || e.kind == "attempt_state")
        .collect();
    rows.chunks(2)
        .map(|pair| {
            assert_eq!(pair[0].kind, "run", "{rows:#?}");
            assert_eq!(pair[1].kind, "attempt_state", "{rows:#?}");
            (
                pair[0].reference.clone(),
                pair[1].hash.clone().unwrap_or_default(),
            )
        })
        .collect()
}

async fn improve_outcome(h: &Harness, item: &str) -> Option<String> {
    h.events(item)
        .await
        .iter()
        .filter_map(crate::controller::load::decode_rung)
        .find(|r| r.rung == Rung::Improve)
        .map(|r| r.outcome)
}

#[tokio::test]
async fn the_same_failure_in_the_same_state_trips_the_breaker_into_one_investigation() {
    let h = Harness::new(&["pinch"]);
    for _ in 0..12 {
        h.script.push(GAUGE, rejected());
    }
    let x = h.file_one(gauge(generous())).await;
    step_until(&h, 40, || async { improve_outcome(&h, &x).await.is_some() }).await;
    h.ctl.set_draining(true);

    // Three identical failures after the first: the fourth attempt's is
    // the third in the same state, and no fifth identical one is given.
    assert_eq!(h.script.briefs_for(GAUGE).len(), 4);
    assert_eq!(
        h.rungs(&x).await,
        vec![Rung::Repair, Rung::Repair, Rung::Repair, Rung::Improve]
    );
    let runs = attempts(&h, &x).await;
    assert_eq!(runs.len(), 4);
    // The first attempt is the item as filed; the repairs are one state.
    assert_ne!(runs[0].1, runs[1].1);
    assert_eq!(runs[1].1, runs[2].1);
    assert_eq!(runs[2].1, runs[3].1);
    let tripped: Vec<&str> = runs[1..].iter().map(|(r, _)| r.as_str()).collect();

    let fingerprint = last_error(&h, &x).await.fingerprint;
    let outcome = improve_outcome(&h, &x).await.unwrap();
    assert!(outcome.contains("circuit breaker tripped"), "{outcome}");
    for run in &tripped {
        assert!(outcome.contains(run), "{outcome} names {run}");
    }
    let open = evidence(&h, &x, "circuit_open").await;
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].hash.as_deref(), Some(fingerprint.as_str()));
    let record: serde_json::Value = serde_json::from_str(&open[0].reference).unwrap();
    assert_eq!(record["runs"], serde_json::json!(tripped));
    assert_eq!(record["fingerprint"], fingerprint.as_str());
    assert_eq!(record["attempt_state"], runs[3].1.as_str());
    assert_eq!(evidence(&h, &x, "attempt_failure").await.len(), 4);

    // Quarantined behind one investigation through the ladder's own path.
    let fixes = h.of_kind(WorkKind::Internal).await;
    assert_eq!(fixes.len(), 1);
    assert!(fixes[0]
        .artifact_refs
        .iter()
        .any(|r| r.kind == "failure_fingerprint" && r.value == fingerprint));
    assert_eq!(h.status(&x).await, Status::Queued);
    let snap = h.ctl.load().await.unwrap();
    assert!(snap
        .edges_held_by(&x)
        .any(|e| e.kind == EdgeKind::Blocks && e.depends_on == fixes[0].id));
}

#[tokio::test]
async fn a_new_repair_generation_restarts_the_count_and_the_item_resumes() {
    const RAW: &str = "E2E-GAUGE-41 tide gauge frame checksum mismatch";
    let h = Harness::new(&["pinch"]);
    // An unknown failure, and no improvement budget of the item's own:
    // only the breaker files its investigation.
    let mut b = generous();
    b.rungs.improvements = 0;
    for _ in 0..4 {
        h.script
            .push(GAUGE, report(failure(ErrorSubclass::Unclassified, RAW)));
    }
    let x = h.file_one(gauge(b)).await;
    // Hold the investigation until its script is in place: the tick that
    // trips the breaker would otherwise lease it at once.
    step_until(&h, 40, || async { h.script.briefs_for(GAUGE).len() == 4 }).await;
    h.ctl.set_draining(true);
    step_until(&h, 10, || async { improve_outcome(&h, &x).await.is_some() }).await;
    let fix = h.of_kind(WorkKind::Internal).await.pop().unwrap();
    // The investigation lands a classifier rule that replays the failure.
    h.script.push(
        &fix.title,
        report(ResultReport {
            summary: "Added the gauge probe.".to_string(),
            artifacts: vec![rustykrab_core::work::ArtifactRef {
                kind: crate::errors::CLASSIFIER_RULE.to_string(),
                value: "process: e2e-gauge-41".to_string(),
            }],
            ..ResultReport::default()
        }),
    );
    h.ctl.set_draining(false);
    h.drain().await;
    assert_eq!(h.status(&fix.id).await, Status::Done);
    assert_eq!(h.status(&x).await, Status::Done, "it resumed and finished");
    let runs = attempts(&h, &x).await;
    assert_eq!(runs.len(), 5);
    assert_ne!(runs[4].1, runs[3].1, "the landed fix is a new generation");
    let states = evidence(&h, &x, "attempt_state").await;
    let last: serde_json::Value = serde_json::from_str(&states[4].reference).unwrap();
    assert_eq!(last["generation"][0]["item"], fix.id.as_str());
    assert_eq!(h.of_kind(WorkKind::Internal).await.len(), 1);
}

#[tokio::test]
async fn when_no_investigation_can_be_filed_the_item_surfaces_once() {
    let h = Harness::new(&["pinch"]);
    for _ in 0..20 {
        h.script.push(GAUGE, rejected());
    }
    let x = h.file_one(gauge(generous())).await;
    // The investigation concludes without landing anything (the scripted
    // default report), so it waits on its owner; the item goes on, and the
    // next trip has nowhere to file.
    h.drain().await;
    let fixes = h.of_kind(WorkKind::Internal).await;
    assert_eq!(fixes.len(), 1, "one investigation, never a second");
    assert_eq!(
        h.status(&fixes[0].id).await,
        Status::Blocked(BlockedReason::NeedsDecision)
    );
    assert_eq!(
        h.status(&x).await,
        Status::Blocked(BlockedReason::NeedsDecision)
    );
    let q = open_question(&h, &x).await;
    assert!(
        q.text.contains("no investigation could be filed"),
        "{}",
        q.text
    );
    assert_eq!(evidence(&h, &x, "circuit_open").await.len(), 2);
    let briefs = h.script.briefs_for(GAUGE).len();
    assert!(briefs < 10, "bounded well short of its ladder: {briefs}");
    h.drain().await;
    assert_eq!(h.script.briefs_for(GAUGE).len(), briefs, "and stays put");
    assert_eq!(questions_of(&h, &x).await.len(), 1);
}
