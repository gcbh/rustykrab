//! The loop's other paths: approval, verification, the report seam, stalls,
//! the local-model rule, fan-in, discovered drafts, the read views and
//! aging.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::TimeDelta;
use rustykrab_core::work::{
    BlockedReason, CancelReason, EdgeKind, ErrorSubclass, Question, ResultReport, Rung, Status,
    WorkKind,
};
use rustykrab_tools::work_backend::{
    Principal, Provenance, StatusQuery, StatusSelector, ToolState, WorkBackend,
};
use tokio::sync::Notify;

use super::*;
use crate::handle::ControlHandle;

fn approval_config() -> ControllerConfig {
    let mut config = ControllerConfig::default();
    config.approval.id = Some("policy:resources".to_string());
    config.approval.delegated_resources = Some(BTreeSet::from(["calendar:home".to_string()]));
    config
}

fn trip_with_an_undelegated_write() -> WorkPlan {
    let mut mail = draft("m", "Email the landlord");
    mail.writable_resources = vec!["mailbox:work".to_string()];
    plan(vec![
        draft("P", "Move flats"),
        mail,
        draft("c", "Compare vans"),
    ])
}

#[tokio::test]
async fn an_undelegated_write_is_held_asked_once_and_released_by_approve() {
    let h = Harness::with(approval_config(), StaticCatalog::default(), &["pinch"]);
    let accepted = h.file(trip_with_an_undelegated_write()).await;
    let (p, m, c) = (&accepted.ids["P"], &accepted.ids["m"], &accepted.ids["c"]);
    assert_eq!(accepted.held, vec![m.clone()]);
    assert_eq!(accepted.policy.as_deref(), Some("policy:resources"));
    assert_eq!(
        h.status(m).await,
        Status::Blocked(BlockedReason::NeedsConsent)
    );
    assert_eq!(h.status(c).await, Status::Ready, "the unheld sibling runs");
    let outbox = h.outbox().await;
    assert_eq!(outbox.len(), 1);
    assert!(
        outbox[0].body.contains("Needs your approval"),
        "{}",
        outbox[0].body
    );
    assert!(
        outbox[0].body.contains("mailbox:work"),
        "{}",
        outbox[0].body
    );

    h.drain().await;
    assert_eq!(h.status(c).await, Status::Done);
    assert_eq!(
        h.status(m).await,
        Status::Blocked(BlockedReason::NeedsConsent),
        "still held"
    );

    let released = ControlHandle::approve(&h.ctl, p, "user").await.unwrap();
    assert_eq!(released, vec![m.clone()]);
    assert_eq!(h.status(m).await, Status::Ready);
    h.drain().await;
    assert_eq!(h.status(m).await, Status::Done);
    assert_eq!(h.status(p).await, Status::Done);

    // A restart still reads the hold as released.
    let h = h.restart(&["pinch"]);
    h.tick().await;
    assert_eq!(h.status(p).await, Status::Done);
}

#[tokio::test]
async fn reject_cancels_the_held_items_and_the_rest_finishes() {
    let h = Harness::with(approval_config(), StaticCatalog::default(), &["pinch"]);
    let accepted = h.file(trip_with_an_undelegated_write()).await;
    let (p, m) = (&accepted.ids["P"], &accepted.ids["m"]);
    let cancelled = ControlHandle::reject(&h.ctl, p, None, "user")
        .await
        .unwrap();
    assert_eq!(cancelled, vec![m.clone()]);
    assert_eq!(
        h.status(m).await,
        Status::Cancelled(CancelReason::Requested)
    );
    h.drain().await;
    assert_eq!(
        h.status(p).await,
        Status::Done,
        "the declined step is the user's call"
    );
}

#[tokio::test]
async fn a_done_claim_with_an_empty_summary_fails_verification_and_surfaces() {
    let h = Harness::new(&["pinch"]);
    h.script.push(
        "File the taxes",
        report(ResultReport {
            summary: "  ".to_string(),
            ..done("File the taxes")
        }),
    );
    let mut d = draft("x", "File the taxes");
    d.budget = Some(no_rungs());
    let x = h.file_one(d).await;
    h.drain().await;
    assert_eq!(
        h.status(&x).await,
        Status::Blocked(BlockedReason::VerificationFailed)
    );
    assert_eq!(h.rungs(&x).await, vec![Rung::Surface]);
    let outbox = h.outbox().await;
    assert_eq!(outbox.len(), 1);
    assert!(
        outbox[0].body.contains("verification/incomplete"),
        "{}",
        outbox[0].body
    );
    assert!(outbox[0].body.contains("Asked:"), "{}", outbox[0].body);
    // The unverified artifact is recorded, but not as verified evidence.
    let evidence = h.store().work_evidence_list(&x).await.unwrap();
    assert!(evidence
        .iter()
        .filter(|e| e.kind == "path")
        .all(|e| e.verified_by.is_none()));
}

#[tokio::test]
async fn a_question_parks_the_item_as_needs_decision_and_asks_through_the_parent() {
    let h = Harness::new(&["pinch"]);
    h.script.push(
        "Pick a restaurant",
        report(ResultReport {
            summary: "two options".to_string(),
            questions: vec![Question {
                text: "Which one?".to_string(),
                class: "blocking_now".to_string(),
                options: vec!["Tasca".to_string(), "Cervejaria".to_string()],
            }],
            ..ResultReport::default()
        }),
    );
    let ids = h
        .file(plan(vec![
            draft("P", "Dinner on Friday"),
            draft("x", "Pick a restaurant"),
        ]))
        .await
        .ids;
    h.drain().await;
    let x = &ids["x"];
    assert_eq!(
        h.status(x).await,
        Status::Blocked(BlockedReason::NeedsDecision)
    );
    assert_eq!(
        h.status(&ids["P"]).await,
        Status::Blocked(BlockedReason::NeedsDecision)
    );
    let outbox = h.outbox().await;
    assert_eq!(outbox.len(), 1);
    assert_eq!(outbox[0].parent, ids["P"]);
    assert!(
        outbox[0].body.contains("Which one? [Tasca / Cervejaria]"),
        "{}",
        outbox[0].body
    );
}

#[tokio::test]
async fn a_report_is_taken_only_from_the_run_holding_the_lease() {
    let h = Harness::new(&["pinch"]);
    let gate = Arc::new(Notify::new());
    h.script.push(
        "Book the car",
        Step::Wait(gate.clone(), Box::new(done("Book the car"))),
    );
    let x = h.file_one(draft("x", "Book the car")).await;
    h.tick().await;
    assert_eq!(h.status(&x).await, Status::Running);

    let handed = ResultReport {
        summary: "booked through the tool".to_string(),
        ..ResultReport::default()
    };
    let stranger = Provenance {
        conversation_id: None,
        filed_by_item: Some("someone-else".to_string()),
        actor: "worker:pinch".to_string(),
    };
    assert!(
        WorkBackend::report(&h.ctl, x.clone(), handed.clone(), stranger)
            .await
            .is_err()
    );
    let impostor = Provenance {
        conversation_id: None,
        filed_by_item: Some(x.clone()),
        actor: "worker:krabby".to_string(),
    };
    assert!(
        WorkBackend::report(&h.ctl, x.clone(), handed.clone(), impostor)
            .await
            .is_err()
    );
    let holder = Provenance {
        conversation_id: None,
        filed_by_item: Some(x.clone()),
        actor: "worker:pinch".to_string(),
    };
    WorkBackend::report(&h.ctl, x.clone(), handed, holder)
        .await
        .unwrap();

    let t = h.tick().await;
    assert_eq!(t.reconciled, vec![x.clone()]);
    assert_eq!(h.status(&x).await, Status::Done);
    let evidence = h.store().work_evidence_list(&x).await.unwrap();
    assert!(evidence
        .iter()
        .any(|e| e.kind == "summary" && e.reference == "booked through the tool"));
    gate.notify_one();
    h.drain().await;
    assert_eq!(h.status(&x).await, Status::Done);
}

#[tokio::test]
async fn a_lease_past_its_ttl_is_a_stall_the_run_stops_and_the_ladder_retries() {
    let config = ControllerConfig {
        lease_ttl_seconds: 60,
        ..ControllerConfig::default()
    };
    let h = Harness::with(config, StaticCatalog::default(), &["pinch"]);
    let stopped = Arc::new(AtomicBool::new(false));
    h.script
        .push("Scan the receipts", Step::Hang(stopped.clone()));
    let x = h.file_one(draft("x", "Scan the receipts")).await;
    h.tick().await;
    assert_eq!(h.status(&x).await, Status::Running);

    h.clock.advance(TimeDelta::seconds(120));
    let t = h.tick().await;
    assert_eq!(h.rungs(&x).await, vec![Rung::Retry]);
    assert!(t.leased.contains(&x), "retried at once");
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        stopped.load(Ordering::SeqCst),
        "the stalled run was stopped"
    );
    h.drain().await;
    assert_eq!(h.status(&x).await, Status::Done);
}

#[tokio::test]
async fn a_local_worker_runs_one_item_at_a_time_whatever_it_advertises() {
    let h = Harness::with_concurrency(&["solo"], 4);
    let gate = Arc::new(Notify::new());
    h.script.push(
        "Morning digest",
        Step::Wait(gate.clone(), Box::new(done("Morning digest"))),
    );
    let first = h.file_one(draft("a", "Morning digest")).await;
    let second = h.file_one(draft("b", "Water the plants reminder")).await;
    let t = h.tick().await;
    assert_eq!(t.leased, vec![first.clone()]);
    assert_eq!(h.status(&second).await, Status::Ready);
    gate.notify_one();
    h.drain().await;
    assert_eq!(h.status(&second).await, Status::Done);
}

#[tokio::test]
async fn a_downstream_brief_carries_its_inputs_verified_refs_capped() {
    let mut config = ControllerConfig::default();
    config.caps.max_inputs = 1;
    let h = Harness::with(config, StaticCatalog::default(), &["pinch"]);
    let mut c = draft("c", "Pick the cheapest plan");
    c.edges = vec![on(EdgeKind::Blocks, "a"), on(EdgeKind::Blocks, "b")];
    let ids = h
        .file(plan(vec![
            draft("P", "Phone plans"),
            draft("a", "Research plan X"),
            draft("b", "Research plan Y"),
            c,
        ]))
        .await
        .ids;
    h.drain().await;
    let brief = h
        .script
        .briefs_for("Pick the cheapest plan")
        .pop()
        .expect("c ran");
    assert_eq!(brief.inputs.len(), 1, "capped");
    assert_eq!(brief.more_inputs.len(), 1);
    let input = &brief.inputs[0];
    assert!([&ids["a"], &ids["b"]].contains(&&input.item));
    assert_eq!(input.status, Status::Done);
    assert_eq!(input.edge, Some(EdgeKind::Blocks));
    assert!(input.summary.starts_with("did Research plan"));
    assert!(input
        .evidence
        .iter()
        .any(|r| r.kind == "path" && r.value.starts_with("/tmp/research-plan-")));
    // The copied set is recorded with the lease's event trail: the lease was
    // written with the inputs, and the item is done.
    assert_eq!(h.status(&ids["c"]).await, Status::Done);
    assert_eq!(h.status(&ids["P"]).await, Status::Done);
}

#[tokio::test]
async fn discovered_drafts_file_under_the_parent_with_provenance_and_run() {
    let h = Harness::new(&["pinch"]);
    h.script.push(
        "Clean the inbox",
        report(ResultReport {
            discovered: vec![draft("f", "Unsubscribe from the newsletter")],
            ..done("Clean the inbox")
        }),
    );
    let ids = h
        .file(plan(vec![
            draft("P", "Inbox zero"),
            draft("x", "Clean the inbox"),
        ]))
        .await
        .ids;
    h.drain().await;
    let kids = h.store().work_children(&ids["P"]).await.unwrap();
    let found = kids
        .iter()
        .find(|k| k.title == "Unsubscribe from the newsletter")
        .expect("filed under the parent");
    let edges = h.store().work_edges_of(&found.id).await.unwrap();
    assert!(edges
        .iter()
        .any(|e| e.kind == EdgeKind::DiscoveredFrom && e.depends_on == ids["x"]));
    assert_eq!(found.status, Status::Done);
    assert_eq!(h.status(&ids["P"]).await, Status::Done);
}

#[tokio::test]
async fn a_worker_draft_through_work_file_goes_under_its_parent() {
    let h = Harness::new(&[]);
    // Without spend records a parent's envelope is committed by its open
    // children, so a follow-up needs room left under the parent.
    let mut p = draft("P", "Garden");
    p.budget = Some(Budget {
        tokens: 1_000_000,
        iterations: 100,
        wall_seconds: 36_000,
        ..Budget::default()
    });
    let mut x = draft("x", "Mow");
    x.budget = Some(Budget::default());
    let ids = h.file(plan(vec![p, x])).await.ids;
    let outcome = WorkBackend::file(
        &h.ctl,
        draft("t", "Sharpen the blades"),
        Provenance {
            conversation_id: None,
            filed_by_item: Some(ids["x"].clone()),
            actor: "worker:pinch".to_string(),
        },
    )
    .await
    .unwrap();
    let PlanOutcome::Accepted(a) = outcome else {
        panic!("{outcome:?}");
    };
    let filed = h.item(&a.ids["t"]).await;
    assert_eq!(filed.parent.as_deref(), Some(ids["P"].as_str()));
}

#[tokio::test]
async fn a_rejected_filing_stores_nothing_but_its_rejection() {
    let h = Harness::new(&[]);
    let mut a = draft("a", "A");
    a.edges = vec![on(EdgeKind::Blocks, "b")];
    let mut b = draft("b", "B");
    b.edges = vec![on(EdgeKind::Blocks, "a")];
    let outcome = ControlHandle::file_plan(
        &h.ctl,
        plan(vec![draft("P", "Loop"), a, b]),
        Provenance::default(),
        FilingSource::Planner,
    )
    .await
    .unwrap();
    let PlanOutcome::Rejected(r) = outcome else {
        panic!("{outcome:?}");
    };
    assert!(r
        .failed
        .iter()
        .any(|f| f.reason == rustykrab_core::work::RejectionReason::Cycle));
    assert!(h
        .store()
        .work_list(&rustykrab_store::WorkFilter {
            include_closed: true,
            ..Default::default()
        })
        .await
        .unwrap()
        .is_empty());
    assert!(h.outbox().await.is_empty());
}

#[tokio::test]
async fn status_and_graph_views_show_the_tree_with_roll_ups() {
    let h = Harness::new(&[]);
    let mut b = draft("b", "Second");
    b.edges = vec![on(EdgeKind::WaitsFor, "a")];
    let ids = h
        .file(plan(vec![draft("P", "Errands"), draft("a", "First"), b]))
        .await
        .ids;
    let views = WorkBackend::status(
        &h.ctl,
        StatusQuery {
            select: StatusSelector::Root(ids["P"].clone()),
            include_closed: false,
        },
        &Principal::default(),
    )
    .await
    .unwrap();
    assert_eq!(views.len(), 3);
    assert_eq!(views[0].item.id, ids["P"]);
    assert_eq!(views[0].rollup, Some(Status::Running));
    assert_eq!(views[0].children_total, 2);
    let a_view = views.iter().find(|v| v.item.id == ids["a"]).unwrap();
    assert!(a_view
        .edges
        .iter()
        .any(|e| e.item == ids["b"] && e.kind == EdgeKind::WaitsFor));

    let graph = ControlHandle::graph(&h.ctl, &ids["P"]).await.unwrap();
    assert_eq!(graph.nodes.len(), 3);
    assert_eq!(graph.nodes[0].depth, 0);
    assert!(graph.nodes[1..].iter().all(|n| n.depth == 1));
    assert_eq!(
        WorkBackend::tool_state(&h.ctl, "anything"),
        ToolState::Unknown
    );
}

#[tokio::test]
async fn closed_trees_age_into_the_archive_at_idle() {
    let h = Harness::new(&["pinch"]);
    let ids = h
        .file(plan(vec![draft("P", "Old errand"), draft("a", "Do it")]))
        .await
        .ids;
    h.drain().await;
    assert_eq!(h.status(&ids["P"]).await, Status::Done);
    h.clock.advance(TimeDelta::days(31));
    let t = h.tick().await;
    let mut archived = t.archived.clone();
    archived.sort();
    let mut expected = vec![ids["P"].clone(), ids["a"].clone()];
    expected.sort();
    assert_eq!(archived, expected);
    let graph = ControlHandle::graph(&h.ctl, &ids["P"]).await.unwrap();
    assert!(graph.nodes[0].archived_summary.is_some());
}

#[tokio::test]
async fn a_credential_gap_with_a_plan_b_runs_the_plan_b_before_asking() {
    let h = Harness::new(&["pinch"]);
    h.script.push(
        "Switch the carrier",
        report(failure(
            ErrorSubclass::Credential,
            "needs credential: carrier login",
        )),
    );
    let mut b = draft("b", "Draft the switch for the user");
    b.edges = vec![on(EdgeKind::ConditionalOnFailure, "a")];
    let ids = h
        .file(plan(vec![
            draft("P", "Cheaper phone plan"),
            draft("a", "Switch the carrier"),
            b,
        ]))
        .await
        .ids;
    h.drain().await;
    assert_eq!(h.status(&ids["a"]).await, Status::Failed);
    assert_eq!(h.rungs(&ids["a"]).await, vec![Rung::PlanB]);
    assert_eq!(h.status(&ids["b"]).await, Status::Done);
    assert!(h.of_kind(WorkKind::Capability).await.is_empty());
}

#[tokio::test]
async fn a_credential_gap_without_a_plan_b_parks_behind_an_acquire_item_and_asks() {
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
    assert_eq!(
        h.status(&x).await,
        Status::Blocked(BlockedReason::NeedsCredential)
    );
    let caps = h.of_kind(WorkKind::Capability).await;
    assert_eq!(caps.len(), 1);
    assert_eq!(caps[0].title, "Acquire credential: bank login");
    assert_eq!(caps[0].status, Status::Queued, "waits on the credential");
    let outbox = h.outbox().await;
    assert_eq!(outbox.len(), 1);
    assert!(outbox[0].body.contains("bank login"), "{}", outbox[0].body);
}

#[tokio::test]
async fn a_catalog_tool_that_exists_is_acquired_not_built() {
    let mut catalog = StaticCatalog::default();
    catalog
        .tools
        .insert("pdf_render".to_string(), ToolState::RegisteredUnloaded);
    let h = Harness::with(ControllerConfig::default(), catalog, &["pinch"]);
    h.script.push(
        "Render the invoice",
        report(failure(ErrorSubclass::ToolGap, "needs tool: pdf_render")),
    );
    let x = h.file_one(draft("x", "Render the invoice")).await;
    h.step().await;
    h.step().await;
    assert_eq!(h.rungs(&x).await, vec![Rung::Acquire]);
    assert_eq!(
        h.of_kind(WorkKind::Capability).await[0].title,
        "Acquire tool: pdf_render"
    );
}
