//! Plan section 15's scenarios, as far as Phase 1 takes them: 1, 6, 9, 13,
//! 14, 20, 21, 23, 24 and 32.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::TimeDelta;
use rustykrab_core::work::{
    BlockedReason, CancelReason, Edge, EdgeKind, ErrorSubclass, EventKind, Evidence, Rung, Status,
    WorkKind,
};
use rustykrab_core::Error;
use tokio::sync::Notify;

use super::*;
use crate::handle::ControlHandle;

// ── 1 ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn scenario_01_a_personal_task_is_leased_run_verified_and_reported_once_with_evidence() {
    let h = Harness::new(&["pinch"]);
    let mut d = draft("dentist", "Book the dentist");
    d.required_tools = vec!["calendar_read".to_string()];
    let id = h.file_one(d).await;
    assert_eq!(h.status(&id).await, Status::Ready, "nothing gates it");

    let t = h.tick().await;
    assert_eq!(t.leased, vec![id.clone()]);
    let events = h.events(&id).await;
    assert!(
        events
            .iter()
            .any(|e| e.kind == EventKind::Lease && e.actor == "worker:pinch"),
        "the lease event names the worker"
    );

    let t = h.drain().await;
    assert!(t.reconciled.contains(&id));
    assert_eq!(h.status(&id).await, Status::Done);

    // The toolset was named up front, and the brief is typed.
    let briefs = h.script.briefs_for("Book the dentist");
    assert_eq!(briefs.len(), 1);
    assert_eq!(briefs[0].required_tools, vec!["calendar_read".to_string()]);
    assert_eq!(briefs[0].origin_conversation_id.as_deref(), Some("conv-1"));
    assert!(briefs[0].last_error.is_none());

    // The artifact is evidence, verified by the report; the summary is kept.
    let evidence = h.store().work_evidence_list(&id).await.unwrap();
    assert!(evidence.iter().any(|e| e.kind == "path"
        && e.reference == "/tmp/book-the-dentist.txt"
        && e.verified_by.as_deref() == Some("result_report")));
    assert!(evidence.iter().any(|e| e.kind == "summary"));

    // One notice, for the item as its own parent, with the evidence.
    let outbox = h.outbox().await;
    assert_eq!(outbox.len(), 1, "{outbox:#?}");
    assert_eq!(outbox[0].parent, id);
    assert_eq!(outbox[0].origin.as_deref(), Some(id.as_str()));
    assert_eq!(outbox[0].channel, "default");
    let body = &outbox[0].body;
    assert!(body.contains("\"Book the dentist\""), "{body}");
    assert!(body.contains(": done."), "{body}");
    assert!(body.contains("path:/tmp/book-the-dentist.txt"), "{body}");
}

// ── 6 ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn scenario_06_two_items_writing_one_calendar_never_run_together() {
    let h = Harness::new(&["pinch", "krabby"]);
    let gate = Arc::new(Notify::new());
    h.script.push(
        "Add the dentist",
        Step::Wait(gate.clone(), Box::new(done("Add the dentist"))),
    );
    let mut first = draft("a", "Add the dentist");
    first.writable_resources = vec!["calendar:home".to_string()];
    let mut second = draft("b", "Add the gym");
    second.writable_resources = vec!["calendar:home".to_string()];
    let a = h.file_one(first).await;
    let b = h.file_one(second).await;
    let free = h.file_one(draft("c", "Read the news")).await;

    let t = h.tick().await;
    assert_eq!(
        t.leased,
        vec![a.clone(), free.clone()],
        "krabby is free, but the calendar is not"
    );
    assert_eq!(h.status(&b).await, Status::Ready);
    // Still not while the first writer runs, tick after tick.
    for _ in 0..3 {
        let t = h.step().await;
        assert!(!t.leased.contains(&b));
        assert_eq!(h.status(&b).await, Status::Ready);
    }

    gate.notify_one();
    h.drain().await;
    for id in [&a, &b, &free] {
        assert_eq!(h.status(id).await, Status::Done);
    }
    assert_eq!(h.script.peak("calendar:home"), 1);
}

// ── 9 ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn scenario_09_an_item_expires_the_user_is_told_and_nothing_runs() {
    let h = Harness::new(&["pinch"]);
    let mut d = draft("bill", "Pay the electricity bill");
    d.expires_at = Some(h.clock.now() + TimeDelta::hours(1));
    let id = h.file_one(d).await;

    h.clock.advance(TimeDelta::hours(2));
    let t = h.tick().await;
    assert_eq!(t.expired, vec![id.clone()]);
    assert!(t.leased.is_empty());
    assert_eq!(h.status(&id).await, Status::Expired);

    h.drain().await;
    assert!(h.script.briefs().is_empty(), "nothing ran");
    let outbox = h.outbox().await;
    assert_eq!(outbox.len(), 1, "told once: {outbox:#?}");
    assert_eq!(outbox[0].parent, id);
    assert!(outbox[0].body.contains("expired"), "{}", outbox[0].body);
}

// ── 13 ─────────────────────────────────────────────────────────────────

/// A catalog that learns a tool when told the build wrote it, and
/// counts the refreshes that asked (section 8, rung 2b: the daemon loads
/// what a build wrote at run time).
#[derive(Default)]
struct Learning {
    built: std::sync::Mutex<Vec<String>>,
    written: std::sync::Mutex<Vec<String>>,
    refreshes: std::sync::atomic::AtomicUsize,
}

impl Learning {
    fn write(&self, tool: &str) {
        self.written.lock().unwrap().push(tool.to_string());
    }
}

impl super::super::ToolCatalog for Learning {
    fn tool_state(&self, name: &str) -> rustykrab_tools::work_backend::ToolState {
        if self.built.lock().unwrap().iter().any(|t| t == name) {
            rustykrab_tools::work_backend::ToolState::RegisteredUnloaded
        } else {
            rustykrab_tools::work_backend::ToolState::Unknown
        }
    }

    fn mcp_server_configured(&self, _name: &str) -> bool {
        false
    }

    fn refresh(&self) {
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        let written = self.written.lock().unwrap().clone();
        *self.built.lock().unwrap() = written;
    }
}

#[tokio::test]
async fn scenario_13_a_missing_tool_files_a_capability_build_and_the_original_resumes_with_it() {
    let catalog = Arc::new(Learning::default());
    let h = Harness::with_catalog(
        crate::controller::ControllerConfig::default(),
        catalog.clone(),
        &["pinch"],
    );
    h.script.push(
        "Render the invoice",
        report(failure(ErrorSubclass::ToolGap, "needs tool: pdf_render")),
    );
    let x = h.file_one(draft("x", "Render the invoice")).await;

    h.step().await;
    h.step().await;
    // The ladder parked the item behind a capability item.
    assert_eq!(
        h.status(&x).await,
        Status::Blocked(BlockedReason::NeedsTool)
    );
    let caps = h.of_kind(WorkKind::Capability).await;
    assert_eq!(caps.len(), 1);
    assert_eq!(caps[0].title, "Build tool: pdf_render");
    assert!(caps[0].parent.is_none(), "not part of the original's chain");
    // The need is its ref; that it is a build is its facet, which the
    // projection, routing and the verifier all read.
    assert!(
        caps[0]
            .artifact_refs
            .iter()
            .any(|r| r.kind == "capability" && r.value == "tool:pdf_render"),
        "{:?}",
        caps[0].artifact_refs
    );
    assert_eq!(
        h.store()
            .work_facets_get(&caps[0].id)
            .await
            .unwrap()
            .and_then(|f| f.capability),
        Some(rustykrab_core::work::CapabilityMode::Build)
    );
    let edges = h.store().work_edges_of(&x).await.unwrap();
    assert!(edges.contains(&Edge {
        item: x.clone(),
        depends_on: caps[0].id.clone(),
        kind: EdgeKind::Blocks,
    }));
    assert_eq!(h.rungs(&x).await, vec![Rung::Build]);
    assert!(
        catalog.refreshes.load(Ordering::SeqCst) > 0,
        "looked again first"
    );

    // A build that reports done without the tool existing is a claim
    // beyond the evidence: it fails verification and repairs.
    h.step().await;
    h.step().await;
    let events = h.events(&caps[0].id).await;
    assert!(
        events
            .iter()
            .any(|e| e.to == Some(Status::Blocked(BlockedReason::VerificationFailed))),
        "{events:#?}"
    );

    // The build writes the tool; the repaired run verifies it, and the
    // original resumes with the tool activated up front and the build's
    // report among its inputs.
    catalog.write("pdf_render");
    h.drain().await;
    assert_eq!(h.status(&caps[0].id).await, Status::Done);
    let evidence = h.store().work_evidence_list(&caps[0].id).await.unwrap();
    assert!(evidence.iter().any(|e| e.kind == "tool"
        && e.reference == "pdf_render"
        && e.verified_by.as_deref() == Some("catalog")));
    assert_eq!(h.status(&x).await, Status::Done);
    let briefs = h.script.briefs_for("Render the invoice");
    assert_eq!(briefs.len(), 2);
    assert!(briefs[1].required_tools.contains(&"pdf_render".to_string()));
    assert!(briefs[1].last_error.is_some());
    assert!(
        briefs[1].inputs.iter().any(|i| i.item == caps[0].id),
        "the resumed brief carries the build: {:?}",
        briefs[1].inputs
    );

    // The user was never asked: one notice, the final report.
    let outbox = h.outbox().await;
    assert_eq!(outbox.len(), 1, "{outbox:#?}");
    assert_eq!(outbox[0].parent, x);
    assert!(!outbox[0].body.contains("Asked"), "{}", outbox[0].body);
}

// ── 14 ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn scenario_14_an_unclassifiable_failure_files_an_internal_item_with_its_evidence() {
    let h = Harness::new(&["pinch"]);
    h.script.push(
        "Sync the photos",
        Step::Fail(Error::Internal("flux capacitor desynchronised".to_string())),
    );
    let x = h.file_one(draft("x", "Sync the photos")).await;

    h.step().await;
    h.step().await;
    let internal = h.of_kind(WorkKind::Internal).await;
    assert_eq!(internal.len(), 1);
    let item = &internal[0];
    let rung = h
        .events(&x)
        .await
        .iter()
        .filter_map(crate::controller::load::decode_rung)
        .next()
        .expect("a rung");
    let error = rung.error.expect("the rung carries its error");
    assert_eq!(error.class, rustykrab_core::work::ErrorClass::Unknown);
    assert!(item.title.contains(&error.fingerprint), "{}", item.title);
    assert!(item.objective.contains("flux capacitor desynchronised"));
    assert!(item
        .artifact_refs
        .iter()
        .any(|r| r.kind == "item" && r.value == x));
    // Order 3 files the improvement and the ladder goes on: a repair.
    assert_eq!(h.rungs(&x).await, vec![Rung::Improve, Rung::Repair]);

    h.drain().await;
    assert_eq!(h.status(&x).await, Status::Done);
    let briefs = h.script.briefs_for("Sync the photos");
    assert_eq!(briefs.len(), 2);
    let note = briefs[1].last_error.clone().unwrap_or_default();
    assert!(note.contains("unknown/unclassified"), "{note}");
    assert!(
        briefs[1].prior_evidence.iter().any(|e| e.kind == "error"),
        "the repair carries the failure's evidence"
    );
}

/// A rule report for the internal item: `classifier_rule` `<subclass>:
/// <pattern>`.
fn landing(rule: &str) -> Step {
    report(rustykrab_core::work::ResultReport {
        summary: "Added the probe.".to_string(),
        artifacts: vec![rustykrab_core::work::ArtifactRef {
            kind: crate::errors::CLASSIFIER_RULE.to_string(),
            value: rule.to_string(),
        }],
        ..rustykrab_core::work::ResultReport::default()
    })
}

fn first_error(events: &[rustykrab_core::work::WorkEvent]) -> rustykrab_core::work::WorkError {
    events
        .iter()
        .filter_map(crate::controller::load::decode_rung)
        .find_map(|r| r.error)
        .expect("a rung with its error")
}

#[tokio::test]
async fn scenario_14_the_internal_items_rule_lands_after_replay_and_classifies_the_failure() {
    const RAW: &str = "E2E-ZQX-17 flux capacitor desynchronised";
    let h = Harness::new(&["pinch"]);
    for title in [
        "Reconcile the ledger",
        "Reconcile it again",
        "And once more",
    ] {
        for _ in 0..6 {
            h.script
                .push(title, report(failure(ErrorSubclass::Unclassified, RAW)));
        }
    }
    let x = h.file_one(draft("x", "Reconcile the ledger")).await;
    h.step().await;
    h.step().await;
    assert_eq!(
        first_error(&h.events(&x).await).class,
        rustykrab_core::work::ErrorClass::Unknown
    );
    let internal = h.of_kind(WorkKind::Internal).await;
    assert_eq!(internal.len(), 1);
    let fix = internal[0].clone();

    // A rule that does not classify the failure fails the report; the
    // repair lands one that does.
    h.script
        .push(&fix.title, landing("process: nothing like this"));
    h.script.push(&fix.title, landing("process: e2e-zqx-17"));
    h.drain().await;
    assert_eq!(h.status(&fix.id).await, Status::Done);
    let refused = first_error(&h.events(&fix.id).await);
    assert_eq!(refused.subclass, ErrorSubclass::ClaimMismatch);
    let kept: Vec<Evidence> = h
        .store()
        .work_evidence_list(&fix.id)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == crate::errors::CLASSIFIER_RULE)
        .collect();
    // The refused rule stays as the failed attempt's unverified evidence;
    // only the replayed one is verified, and only that one is consulted.
    let verified: Vec<&str> = kept
        .iter()
        .filter(|e| e.verified_by.as_deref() == Some("replay"))
        .map(|e| e.reference.as_str())
        .collect();
    assert_eq!(verified, ["process: e2e-zqx-17"], "{kept:?}");
    assert_eq!(kept.len(), 2);

    // The same failure on a new item is classified by the landed rule.
    let y = h.file_one(draft("y", "Reconcile it again")).await;
    h.drain().await;
    let again = first_error(&h.events(&y).await);
    assert_eq!(again.subclass, ErrorSubclass::Process);
    assert!(again.observed_by.starts_with("rule:learned:"), "{again:?}");

    // And after a restart, from the evidence alone.
    let h = h.restart(&["pinch"]);
    let z = h.file_one(draft("z", "And once more")).await;
    h.drain().await;
    assert_eq!(
        first_error(&h.events(&z).await).subclass,
        ErrorSubclass::Process
    );
}

// ── 20 ─────────────────────────────────────────────────────────────────

fn chain() -> WorkPlan {
    let mut b = draft("b", "Compare the fares");
    b.edges = vec![on(EdgeKind::Blocks, "a")];
    let mut c = draft("c", "Book the cheapest");
    c.edges = vec![on(EdgeKind::Blocks, "b")];
    plan(vec![
        draft("P", "Plan the Lisbon trip"),
        draft("a", "Collect the dates"),
        b,
        c,
    ])
}

#[tokio::test]
async fn scenario_20_a_failing_middle_item_climbs_first_then_holds_the_chain_and_surfaces_once() {
    let h = Harness::new(&["pinch", "krabby"]);
    for _ in 0..10 {
        h.script.push(
            "Compare the fares",
            report(failure(ErrorSubclass::Timeout, "fare search timed out")),
        );
    }
    let ids = h.file(chain()).await.ids;
    let (p, a, b, c) = (&ids["P"], &ids["a"], &ids["b"], &ids["c"]);

    for _ in 0..60 {
        h.step().await;
        if h.status(b).await == Status::Failed {
            break;
        }
        // Nothing downstream moves while b climbs.
        assert_eq!(h.status(c).await, Status::Queued);
        assert!(h.outbox().await.is_empty(), "no message while b climbs");
    }
    assert_eq!(
        h.rungs(b).await,
        vec![
            Rung::Retry,
            Rung::Retry,
            Rung::Repair,
            Rung::Repair,
            Rung::SwitchWorker,
            Rung::Replan,
        ]
    );
    // The switch moved b to the other worker.
    let workers: Vec<String> = h
        .script
        .briefs()
        .into_iter()
        .filter(|(_, br)| br.title == "Compare the fares")
        .map(|(w, _)| w)
        .collect();
    assert_ne!(workers.first(), workers.last(), "{workers:?}");

    assert_eq!(h.status(a).await, Status::Done);
    assert_eq!(h.status(b).await, Status::Failed);
    assert_eq!(
        h.status(c).await,
        Status::Blocked(BlockedReason::UpstreamFailed)
    );
    assert_eq!(h.origin(c).await.as_deref(), Some(b.as_str()));
    assert_eq!(
        h.status(p).await,
        Status::Blocked(BlockedReason::UpstreamFailed)
    );
    assert_eq!(h.origin(p).await.as_deref(), Some(b.as_str()));

    h.drain().await;
    let outbox = h.outbox().await;
    assert_eq!(
        outbox.len(),
        1,
        "one message however much is held: {outbox:#?}"
    );
    assert_eq!(&outbox[0].parent, p);
    let body = &outbox[0].body;
    assert!(body.contains("\"Plan the Lisbon trip\""), "{body}");
    assert!(body.contains("Not done: \"Compare the fares\""), "{body}");
    assert!(body.contains("reached order 3+"), "{body}");
    assert!(body.contains("tool/timeout"), "{body}");
    assert!(body.contains("Held: \"Book the cheapest\""), "{body}");
}

// ── 21 ─────────────────────────────────────────────────────────────────

fn with_plan_b() -> WorkPlan {
    let mut a = draft("a", "Book the hotel online");
    a.budget = Some(no_rungs());
    let mut b = draft("b", "Book the hotel by email");
    b.edges = vec![on(EdgeKind::ConditionalOnFailure, "a")];
    plan(vec![draft("P", "Find a hotel"), a, b])
}

#[tokio::test]
async fn scenario_21_plan_b_runs_before_any_notice_and_the_user_hears_only_the_result() {
    let h = Harness::new(&["pinch"]);
    h.script.push(
        "Book the hotel online",
        report(failure(
            ErrorSubclass::CheckFailed,
            "the booking page rejected the card",
        )),
    );
    let ids = h.file(with_plan_b()).await.ids;
    let (p, a, b) = (&ids["P"], &ids["a"], &ids["b"]);
    assert_eq!(h.status(b).await, Status::Queued, "a plan B waits");

    h.step().await;
    h.step().await;
    assert_eq!(h.status(a).await, Status::Failed);
    assert_eq!(h.rungs(a).await, vec![Rung::PlanB]);
    assert!(matches!(
        h.status(b).await,
        Status::Ready | Status::Leased | Status::Running
    ));
    assert!(h.outbox().await.is_empty(), "plan B runs before any notice");

    h.drain().await;
    assert_eq!(h.status(b).await, Status::Done);
    assert_eq!(h.status(p).await, Status::Done, "plan B stood in for a");
    let outbox = h.outbox().await;
    assert_eq!(outbox.len(), 1, "{outbox:#?}");
    assert_eq!(&outbox[0].parent, p);
    let body = &outbox[0].body;
    assert!(
        body.contains("Plan B \"Book the hotel by email\""),
        "{body}"
    );
    assert!(body.contains("is done"), "{body}");
}

#[tokio::test]
async fn scenario_21_a_plan_b_is_cancelled_unleased_when_its_step_succeeds() {
    let h = Harness::new(&["pinch"]);
    let ids = h.file(with_plan_b()).await.ids;
    let (p, a, b) = (&ids["P"], &ids["a"], &ids["b"]);
    h.drain().await;
    assert_eq!(h.status(a).await, Status::Done);
    assert_eq!(h.status(b).await, Status::Cancelled(CancelReason::Cascade));
    assert_eq!(h.leases(b).await, 0, "never leased");
    assert!(h.script.briefs_for("Book the hotel by email").is_empty());
    assert_eq!(h.status(p).await, Status::Done);
    assert_eq!(h.outbox().await.len(), 1);
}

// ── 23 ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn scenario_23_cancelling_a_parent_cancels_what_is_open_revokes_the_run_and_keeps_what_finished(
) {
    let h = Harness::new(&["pinch", "krabby"]);
    let stopped = Arc::new(AtomicBool::new(false));
    h.script
        .push("Draft the itinerary", Step::Hang(stopped.clone()));
    let mut q1 = draft("q1", "Book dinner");
    q1.edges = vec![on(EdgeKind::Blocks, "r")];
    let mut q2 = draft("q2", "Plan the museum day");
    q2.edges = vec![on(EdgeKind::Blocks, "r")];
    let mut g = draft("g", "Buy museum tickets");
    g.parent = Some(tmp("q2"));
    let ids = h
        .file(plan(vec![
            draft("P", "Weekend in Porto"),
            draft("d", "Check the passports"),
            draft("r", "Draft the itinerary"),
            q1,
            q2,
            g,
        ]))
        .await
        .ids;
    let (p, d, r) = (&ids["P"], &ids["d"], &ids["r"]);

    h.step().await;
    h.step().await;
    assert_eq!(h.status(d).await, Status::Done);
    assert_eq!(h.status(r).await, Status::Running);
    h.store()
        .work_evidence_add(Evidence {
            item: r.clone(),
            kind: "note".to_string(),
            reference: "half an itinerary".to_string(),
            hash: None,
            verified_by: None,
            at: h.clock.now(),
        })
        .await
        .unwrap();

    let cancelled = ControlHandle::cancel(&h.ctl, p, Some("trip called off".into()), "user")
        .await
        .unwrap();
    for id in [p, r, &ids["q1"], &ids["q2"], &ids["g"]] {
        assert!(cancelled.contains(id), "{id} cancelled");
    }
    assert_eq!(
        h.status(p).await,
        Status::Cancelled(CancelReason::Requested)
    );
    for id in [r, &ids["q1"], &ids["q2"], &ids["g"]] {
        assert_eq!(
            h.status(id).await,
            Status::Cancelled(CancelReason::Cascade),
            "{id}"
        );
        assert_eq!(h.origin(id).await.as_deref(), Some(p.as_str()));
    }
    for id in [&ids["q1"], &ids["q2"], &ids["g"]] {
        assert_eq!(h.leases(id).await, 0, "cancelled without being leased");
    }
    assert_eq!(
        h.status(d).await,
        Status::Done,
        "the done child keeps its status"
    );

    // The run was stopped, its lease revoked, its partial evidence kept.
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(stopped.load(Ordering::SeqCst), "the run was aborted");
    assert!(h.store().work_lease_get(r).await.unwrap().is_none());
    assert!(h
        .store()
        .work_evidence_list(r)
        .await
        .unwrap()
        .iter()
        .any(|e| e.reference == "half an itinerary"));
    assert!(h.ctl.running().is_empty());

    let outbox = h.outbox().await;
    assert_eq!(outbox.len(), 1, "{outbox:#?}");
    let body = &outbox[0].body;
    assert!(body.contains("cancelled (requested)"), "{body}");
    assert!(
        body.contains("Finished before the cancel: \"Check the passports\""),
        "{body}"
    );
}

// ── 24 ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn scenario_24_expiry_holds_blocks_releases_waits_for_and_a_parent_expiry_cancels_all() {
    let h = Harness::new(&[]);
    let now = h.clock.now();
    let mut p = draft("P", "Concert trip");
    p.expires_at = Some(now + TimeDelta::hours(3));
    let mut a = draft("a", "Buy the tickets");
    a.expires_at = Some(now + TimeDelta::hours(1));
    let mut b = draft("b", "Book the train");
    b.edges = vec![on(EdgeKind::Blocks, "a")];
    let mut c = draft("c", "Tell Ana how it went");
    c.edges = vec![on(EdgeKind::WaitsFor, "a")];
    let q = draft("q", "Pack");
    let ids = h.file(plan(vec![p, a, b, c, q])).await.ids;
    let (pid, a, b, c, q) = (&ids["P"], &ids["a"], &ids["b"], &ids["c"], &ids["q"]);

    h.clock.advance(TimeDelta::hours(2));
    let t = h.tick().await;
    assert_eq!(t.expired, vec![a.clone()]);
    assert_eq!(
        h.status(b).await,
        Status::Blocked(BlockedReason::UpstreamExpired)
    );
    assert_eq!(h.origin(b).await.as_deref(), Some(a.as_str()));
    assert_eq!(h.status(c).await, Status::Ready, "expiry is terminal");
    let outbox = h.outbox().await;
    assert_eq!(outbox.len(), 1, "told once: {outbox:#?}");
    assert_eq!(&outbox[0].parent, pid);
    assert!(
        outbox[0].body.contains("Expired: \"Buy the tickets\""),
        "{}",
        outbox[0].body
    );

    h.clock.advance(TimeDelta::hours(2));
    let t = h.tick().await;
    assert_eq!(t.expired, vec![pid.clone()]);
    assert_eq!(h.status(pid).await, Status::Expired);
    for id in [b, c, q] {
        assert_eq!(
            h.status(id).await,
            Status::Cancelled(CancelReason::Cascade),
            "{id}"
        );
        assert_eq!(h.origin(id).await.as_deref(), Some(pid.as_str()));
    }
    let outbox = h.outbox().await;
    assert_eq!(outbox.len(), 2);
    assert!(outbox[1].body.contains(": expired;"), "{}", outbox[1].body);
}

// ── 32 ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn scenario_32_a_restart_re_derives_the_same_state_re_plans_nothing_and_notices_once() {
    let h = Harness::new(&["pinch", "krabby"]);
    let stopped = Arc::new(AtomicBool::new(false));
    h.script
        .push("Research the options", Step::Hang(stopped.clone()));
    h.script.push(
        "Fetch the quotes",
        report(failure(
            ErrorSubclass::CheckFailed,
            "the quote page changed",
        )),
    );
    let mut s = draft("s", "Fetch the quotes");
    s.budget = Some(no_rungs());
    let mut held = draft("h", "Pick the cheapest");
    held.edges = vec![on(EdgeKind::Blocks, "s")];
    let ids = h
        .file(plan(vec![
            draft("P", "Switch the phone plan"),
            s,
            held,
            draft("l", "Research the options"),
        ]))
        .await
        .ids;
    let (p, s, held, l) = (
        ids["P"].clone(),
        ids["s"].clone(),
        ids["h"].clone(),
        ids["l"].clone(),
    );

    h.step().await;
    h.step().await;
    assert_eq!(h.status(&s).await, Status::Failed);
    assert_eq!(
        h.status(&held).await,
        Status::Blocked(BlockedReason::UpstreamFailed)
    );
    assert_eq!(h.status(&l).await, Status::Running);
    let pending = h.outbox().await;
    assert_eq!(pending.len(), 1, "the parent's message is pending");
    let before = h.store().work_list(&Default::default()).await.unwrap();

    // Restart: a new controller over the same store.
    let h = h.restart(&["pinch"]);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        stopped.load(Ordering::SeqCst),
        "the old run died with its process"
    );
    let t = h.tick().await;

    // The leased child returned to ready (a resume event, nothing failed)
    // and was leased again.
    let resumed = h.events(&l).await;
    assert!(resumed
        .iter()
        .any(|e| e.kind == EventKind::Resume && e.to == Some(Status::Ready)));
    assert!(t.leased.contains(&l));
    assert_eq!(h.leases(&l).await, 2);
    // Everything else re-derives to what it was.
    assert_eq!(h.status(&s).await, Status::Failed);
    assert_eq!(
        h.status(&held).await,
        Status::Blocked(BlockedReason::UpstreamFailed)
    );
    assert_eq!(h.origin(&held).await.as_deref(), Some(s.as_str()));
    assert_eq!(
        h.status(&p).await,
        before.iter().find(|i| i.id == p).unwrap().status
    );
    // Nothing re-planned, no defect found, the message still exactly once.
    assert_eq!(
        h.store()
            .work_list(&Default::default())
            .await
            .unwrap()
            .len(),
        before.len()
    );
    assert!(h.of_kind(WorkKind::Internal).await.is_empty());
    let outbox = h.outbox().await;
    assert_eq!(outbox.len(), 1);
    assert_eq!(outbox[0].id, pending[0].id);
}

#[tokio::test]
async fn scenario_32_a_torn_cascade_is_corrected_with_a_resume_event_and_an_internal_item() {
    let h = Harness::new(&["pinch"]);
    h.script.push(
        "Fetch the quotes",
        report(failure(
            ErrorSubclass::CheckFailed,
            "the quote page changed",
        )),
    );
    let mut s = draft("s", "Fetch the quotes");
    s.budget = Some(no_rungs());
    let mut held = draft("h", "Pick the cheapest");
    held.edges = vec![on(EdgeKind::Blocks, "s")];
    let ids = h
        .file(plan(vec![draft("P", "Switch the phone plan"), s, held]))
        .await
        .ids;
    h.drain().await;
    let held = ids["h"].clone();
    assert_eq!(
        h.status(&held).await,
        Status::Blocked(BlockedReason::UpstreamFailed)
    );

    // A write that skipped its cascade, as a crash in another writer would
    // leave it.
    h.store()
        .work_transition(
            &held,
            None,
            Status::Queued,
            "test",
            Some("torn"),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    let h = h.restart(&["pinch"]);
    h.tick().await;
    assert_eq!(
        h.status(&held).await,
        Status::Blocked(BlockedReason::UpstreamFailed)
    );
    assert_eq!(h.origin(&held).await.as_deref(), Some(ids["s"].as_str()));
    assert!(h
        .events(&held)
        .await
        .iter()
        .any(|e| e.kind == EventKind::Resume
            && e.to == Some(Status::Blocked(BlockedReason::UpstreamFailed))));
    let internal = h.of_kind(WorkKind::Internal).await;
    assert_eq!(internal.len(), 1);
    assert!(internal[0].objective.contains(&held));
}
