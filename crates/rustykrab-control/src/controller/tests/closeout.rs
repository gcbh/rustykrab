//! Phase 1 close-out: approval clears `held_by`, spend per run and budgets
//! from actual spend, unneeded capability items, the interactive-turn gate
//! of 12.1, and a scheduled firing running in its job's conversation.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use rustykrab_core::work::{
    BlockedReason, Budget, CancelReason, ErrorClass, ErrorSubclass, Status, WorkKind,
};

use super::*;
use crate::controller::ModelActivity;
use crate::errors::BudgetKind;
use crate::handle::ControlHandle;
use crate::worker::RunFailure;

/// A model's busy flag, flipped by the test.
#[derive(Default)]
struct Turn(AtomicBool);

impl ModelActivity for Turn {
    fn busy(&self, model: &str) -> bool {
        model == "model-of-pinch" && self.0.load(Ordering::SeqCst)
    }
}

impl Harness {
    fn with_activity(self, activity: Arc<dyn ModelActivity>) -> Harness {
        let Harness {
            ctl,
            clock,
            script,
            store,
            config,
            catalog,
            dir,
        } = self;
        drop(ctl);
        let ctl = Controller::new(
            store.clone(),
            vec![Arc::new(Scripted {
                name: "pinch".to_string(),
                concurrency: 1,
                script: Arc::clone(&script),
            }) as Arc<dyn Worker>],
            config.clone(),
        )
        .with_clock(clock.clone())
        .with_catalog(catalog.clone())
        .with_activity(activity);
        Harness {
            ctl,
            clock,
            script,
            store,
            config,
            catalog,
            dir,
        }
    }
}

fn budget(iterations: u32, tokens: u64, wall_seconds: u64) -> Budget {
    Budget {
        iterations,
        tokens,
        wall_seconds,
        ..Budget::default()
    }
}

#[tokio::test]
async fn approve_clears_held_by_in_the_store() {
    let mut config = ControllerConfig::default();
    config.approval.id = Some("policy:resources".to_string());
    config.approval.delegated_resources = Some(BTreeSet::from(["calendar:home".to_string()]));
    let h = Harness::with(config, StaticCatalog::default(), &["pinch"]);
    let mut mail = draft("m", "Email the landlord");
    mail.writable_resources = vec!["mailbox:work".to_string()];
    let accepted = h.file(plan(vec![draft("P", "Move flats"), mail])).await;
    let (p, m) = (&accepted.ids["P"], &accepted.ids["m"]);
    assert!(h.item(m).await.held_by.is_some(), "held for the question");

    ControlHandle::approve(&h.ctl, p, "user").await.unwrap();
    assert_eq!(h.item(m).await.held_by, None, "the store row is released");
    assert!(h.events(m).await.iter().any(|e| e
        .reason
        .as_deref()
        .is_some_and(|r| r.starts_with("approved "))));
}

#[tokio::test]
async fn each_run_records_its_spend_and_a_run_event_with_its_reminders() {
    let h = Harness::new(&["pinch"]);
    let x = h.file_one(draft("x", "Water the plants")).await;
    h.drain().await;
    assert_eq!(h.status(&x).await, Status::Done);

    let spent = h.store().work_spend_of(&x).await.unwrap();
    assert_eq!(spent.runs, 1);
    assert_eq!(spent.tokens, SCRIPTED_USAGE.tokens);
    assert_eq!(spent.wall_ms, SCRIPTED_USAGE.wall_ms);
    assert_eq!(spent.iterations, u64::from(SCRIPTED_USAGE.iterations));
    assert_eq!(h.ctl.spent_of(&x), spent, "the controller keeps it current");

    let runs: Vec<_> = h
        .events(&x)
        .await
        .into_iter()
        .filter(|e| e.kind == EventKind::Run)
        .collect();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].actor, "worker:pinch");
    assert!(
        runs[0]
            .reason
            .as_deref()
            .is_some_and(|r| r.contains("2 completion reminders")),
        "{:?}",
        runs[0].reason
    );
    let run_evidence: Vec<String> = h
        .store()
        .work_evidence_list(&x)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "run")
        .map(|e| e.reference)
        .collect();
    assert_eq!(runs[0].evidence_ref.as_ref(), run_evidence.first());

    // The lease that ran it stays readable, with what it was given.
    let leases = h.store().work_lease_history(&x).await.unwrap();
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0].lease.worker, "pinch");
    assert!(leases[0].released_at.is_some());
}

#[tokio::test]
async fn a_follow_up_under_a_busy_parent_is_judged_by_spend_not_allocation() {
    let h = Harness::new(&["pinch"]);
    let mut parent = draft("P", "Plan the move");
    parent.budget = Some(budget(10, 10_000, 600));
    let mut a = draft("a", "Book the van");
    a.budget = Some(budget(5, 5_000, 300));
    let mut b = draft("b", "Pack the kitchen");
    b.budget = Some(budget(5, 5_000, 300));
    let accepted = h.file(plan(vec![parent, a, b])).await;
    let p = accepted.ids["P"].clone();

    // Nothing has run: the open children's allocations fill the envelope,
    // but nothing is spent, so a follow-up fits.
    let follow_up = |title: &str, tokens: u64| {
        let mut d = draft("f", title);
        d.budget = Some(budget(1, tokens, 60));
        WorkPlan {
            root: ItemRef::Id(p.clone()),
            items: vec![d],
            edges: Vec::new(),
            rationale: "a follow-up".to_string(),
        }
    };
    let outcome = ControlHandle::file_plan(
        &h.ctl,
        follow_up("Label the boxes", 2_000),
        Harness::provenance(),
        FilingSource::Planner,
    )
    .await
    .unwrap();
    assert!(
        matches!(outcome, PlanOutcome::Accepted(_)),
        "judged by spend, not by allocation: {outcome:?}"
    );

    // The kitchen never finishes, so the parent stays open; the van and
    // the labels spend 1,000 tokens each, which leaves 8,000.
    h.script.push(
        "Pack the kitchen",
        Step::Hang(Arc::new(AtomicBool::new(false))),
    );
    h.drain().await;
    assert_eq!(h.status(&p).await, Status::Running);
    assert_eq!(h.status(&accepted.ids["a"]).await, Status::Done);
    let file = |title: &str, tokens: u64| {
        ControlHandle::file_plan(
            &h.ctl,
            follow_up(title, tokens),
            Harness::provenance(),
            FilingSource::Planner,
        )
    };
    match file("Clean the flat", 8_500).await.unwrap() {
        PlanOutcome::Rejected(r) => assert!(
            r.failed.iter().any(|f| f.detail.contains("8000 tokens")),
            "{r:?}"
        ),
        PlanOutcome::Accepted(a) => panic!("over what is left: {a:?}"),
    }
    assert!(matches!(
        file("Return the keys", 7_500).await.unwrap(),
        PlanOutcome::Accepted(_)
    ));
}

#[tokio::test]
async fn a_capability_item_nothing_needs_is_cancelled_before_it_is_leased() {
    let h = Harness::new(&["pinch"]);
    h.script.push(
        "Pay the invoice",
        report(failure(
            ErrorSubclass::Credential,
            "needs credential: bank login",
        )),
    );
    let x = h.file_one(draft("x", "Pay the invoice")).await;
    h.drain().await;
    let cap = h.of_kind(WorkKind::Capability).await.remove(0);
    assert_eq!(cap.status, Status::Queued, "waits on the credential");
    assert_eq!(
        h.status(&x).await,
        Status::Blocked(BlockedReason::NeedsCredential)
    );
    let notices = h.outbox().await.len();

    ControlHandle::cancel(&h.ctl, &x, None, "user")
        .await
        .unwrap();
    h.tick().await;
    assert_eq!(
        h.status(&cap.id).await,
        Status::Cancelled(CancelReason::Cascade)
    );
    assert_eq!(h.origin(&cap.id).await.as_deref(), Some(x.as_str()));
    assert_eq!(h.leases(&cap.id).await, 0, "never leased");
    assert_eq!(
        h.outbox().await.len(),
        notices + 1,
        "the user hears of the cancel, not of the housekeeping"
    );

    // A capability item nothing ever waited on is somebody's own request.
    let own = h
        .file_one(WorkItemDraft {
            kind: Some(WorkKind::Capability),
            ..draft("c", "Build a bank-export reader")
        })
        .await;
    h.tick().await;
    assert!(!h.status(&own).await.is_closed());
}

#[tokio::test]
async fn local_leases_wait_while_the_model_serves_an_interactive_turn() {
    let turn = Arc::new(Turn::default());
    let h = Harness::new(&[]).with_activity(turn.clone());
    turn.0.store(true, Ordering::SeqCst);
    let x = h.file_one(draft("x", "Water the plants")).await;
    for _ in 0..3 {
        let report = h.step().await;
        assert!(report.leased.is_empty(), "leased during the turn");
    }
    assert_eq!(h.status(&x).await, Status::Ready);

    turn.0.store(false, Ordering::SeqCst);
    let report = h.step().await;
    assert_eq!(report.leased, vec![x.clone()]);
    h.drain().await;
    assert_eq!(h.status(&x).await, Status::Done);
}

#[tokio::test]
async fn a_scheduled_firing_runs_in_its_jobs_conversation() {
    let h = Harness::new(&["pinch"]);
    let job = h
        .store()
        .jobs()
        .create_job(
            "0 9 * * *",
            "Water the plants",
            None,
            None,
            None,
            "UTC",
            false,
        )
        .await
        .unwrap();
    let conversation = Uuid::new_v4().to_string();
    h.store()
        .jobs()
        .set_conversation_id(&job.id, &conversation)
        .await
        .unwrap();
    let firing = h.file_one(draft("x", "Water the plants")).await;
    let other = h.file_one(draft("y", "Feed the cat")).await;
    h.store()
        .jobs()
        .set_work_item_id(&job.id, &firing)
        .await
        .unwrap();
    h.drain().await;

    let runs = |id: &str| {
        let title = if id == firing {
            "Water the plants"
        } else {
            "Feed the cat"
        };
        h.script
            .briefs_for(title)
            .into_iter()
            .filter_map(|b| b.run)
            .collect::<Vec<_>>()
    };
    assert_eq!(runs(&firing), vec![conversation.clone()]);
    let evidence = h.store().work_evidence_list(&firing).await.unwrap();
    assert!(evidence
        .iter()
        .any(|e| e.kind == "run" && e.reference == conversation));
    // Anything else still runs under a fresh id.
    let fresh = runs(&other);
    assert_eq!(fresh.len(), 1);
    assert_ne!(fresh[0], conversation);
}

#[tokio::test]
async fn a_spent_token_budget_is_classified_as_a_budget_error() {
    let h = Harness::new(&["pinch"]);
    h.script.push(
        "Summarise the inbox",
        Step::Fail(
            RunFailure::Budget {
                budget: BudgetKind::Tokens,
                detail: "12000 of 10000 tokens used before result_report".to_string(),
            }
            .into_error(),
        ),
    );
    let x = h.file_one(draft("x", "Summarise the inbox")).await;
    h.step().await;
    h.step().await;
    let error = h
        .events(&x)
        .await
        .iter()
        .filter_map(decode_rung)
        .find_map(|r| r.error)
        .expect("a rung with its error");
    assert_eq!(error.class, ErrorClass::Budget);
    assert_eq!(error.subclass, ErrorSubclass::Tokens);
}
