//! A synthetic store shaped like a long-lived instance's, for measuring
//! the controller's store time: about 300 work items, about 500 recorded
//! project contexts of up to about 850 KB each, and 33 items waiting for
//! consent while an automatic-start grant is in force. Built in a temp
//! directory; never opens a real instance's store.
//!
//! `#[ignore]`d: it writes about 250 MB. Run it with
//! `cargo test -p rustykrab-control contention_bench -- --ignored --nocapture`.

use std::time::Instant;

use rustykrab_core::work::{
    Budget, Evidence, PlanOutcome, Status, Trigger, WorkItem, WorkItemDraft, WorkKind, WorkerKind,
};
use rustykrab_core::work_preference::WorkStartMode;
use rustykrab_projects::{CreateProject, ProjectId, RevisionAuthor};

use super::*;
use crate::handoff::{ProjectContext, ProjectWork, CONTROLLER, PROJECT_CONTEXT};

fn rest() -> Provenance {
    Provenance {
        conversation_id: None,
        filed_by_item: None,
        actor: "user:master".to_string(),
    }
}

fn proposal(title: &str, objective: &str) -> WorkItemDraft {
    WorkItemDraft {
        kind: Some(WorkKind::Proposal),
        title: title.to_string(),
        objective: objective.to_string(),
        done_when: "the metric moves on replay".to_string(),
        constraints: vec!["Roll back if the metric drops".to_string()],
        ..WorkItemDraft::default()
    }
}

fn done(id: &str) -> WorkItem {
    let at = Utc::now() - TimeDelta::days(2);
    WorkItem {
        id: id.into(),
        kind: WorkKind::Code,
        title: format!("finished {id}"),
        objective: "x".repeat(8_000),
        done_when: "done when".into(),
        constraints: vec![],
        decisions_made: vec![],
        artifact_refs: vec![],
        required_tools: vec![],
        required_mcp_servers: vec![],
        worker_kind: WorkerKind::Any,
        writable_resources: vec![],
        parent: None,
        inputs_from: vec![],
        origin_conversation_id: None,
        trigger: Trigger::Now,
        preconditions: vec![],
        expires_at: None,
        budget: Budget::default(),
        priority: 0,
        status: Status::Done,
        status_origin: None,
        plan_id: None,
        held_by: None,
        created_at: at,
        updated_at: at,
        closed_at: Some(at),
    }
}

/// Build the synthetic store under `h`. Returns the number of context
/// bytes written.
pub(super) async fn synthetic_store(h: &Harness) -> usize {
    // 33 proposals wait for consent; one automatic grant is in force and
    // its daily allowance is already used, as on the live instance.
    let first = match ControlHandle::file_draft(
        &h.ctl,
        proposal("Bounded cleanup", "Change what cleanup says"),
        rest(),
    )
    .await
    .unwrap()
    {
        PlanOutcome::Accepted(a) => a.root,
        PlanOutcome::Rejected(r) => panic!("rejected: {r:?}"),
    };
    ControlHandle::start_work(
        &h.ctl,
        &first,
        WorkStartMode::Automatic,
        1,
        "One a day",
        "user:master",
    )
    .await
    .unwrap();
    for n in 0..33 {
        let outcome = ControlHandle::file_draft(
            &h.ctl,
            proposal(
                &format!("Waiting {n}"),
                &format!("Change what part {n} says"),
            ),
            rest(),
        )
        .await
        .unwrap();
        assert!(matches!(outcome, PlanOutcome::Accepted(_)));
    }
    // About 265 finished items.
    let rows: Vec<_> = (0..265).map(|n| done(&format!("done-{n:03}"))).collect();
    h.store().work_insert_graph(&rows, &[], None).await.unwrap();
    // About 500 recorded project contexts, 100 KB to 850 KB.
    let snapshot = h
        .store()
        .projects()
        .create(CreateProject::new(
            "project",
            ProjectId::new(),
            "Shared project",
            RevisionAuthor::User,
            Utc::now(),
        ))
        .await
        .unwrap()
        .snapshot;
    let mut written = 0;
    for n in 0..500 {
        let item = format!("done-{:03}", n % 250);
        let target = 100_000 + (n * 7_919) % 750_000;
        let entries = target / 8_000 + 1;
        let work: Vec<ProjectWork> = (0..entries)
            .map(|e| ProjectWork {
                item: if e == 0 {
                    item.clone()
                } else {
                    format!("history-{e}")
                },
                title: format!("history {e}"),
                status: Status::Done,
                worker: Some("pinch".into()),
                summary: "s".repeat(7_800),
                objective: String::new(),
                done_when: String::new(),
                constraints: vec![],
                decisions_made: vec![],
                evidence: vec![],
                repository: None,
                unfinished_attempt: None,
            })
            .collect();
        let context = ProjectContext {
            snapshot: snapshot.clone(),
            work,
            base_sources: vec![],
            questions: vec![],
            execution_items: vec![],
            history_items: None,
        };
        let reference = serde_json::to_string(&context).unwrap();
        written += reference.len();
        h.store()
            .work_evidence_add(Evidence {
                item,
                kind: PROJECT_CONTEXT.into(),
                reference,
                hash: Some(snapshot.revision.id.to_string()),
                verified_by: Some(CONTROLLER.into()),
                at: Utc::now(),
            })
            .await
            .unwrap();
    }
    written
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "writes about 250 MB; run by hand to measure"]
async fn contention_bench() {
    let h = Harness::new(&["pinch"]);
    let written = synthetic_store(&h).await;
    eprintln!(
        "synthetic store: {} live items, {:.0} MB of project context rows",
        h.store()
            .work_list(&rustykrab_store::WorkFilter {
                include_closed: true,
                ..Default::default()
            })
            .await
            .unwrap()
            .len(),
        written as f64 / 1e6
    );
    // Warm-up: the first tick reads every recorded context once.
    let began = Instant::now();
    h.tick().await;
    eprintln!("first tick: {} ms", began.elapsed().as_millis());
    let mut ticks = Vec::new();
    let loads = h.ctl.loads.load(std::sync::atomic::Ordering::Relaxed);
    for _ in 0..3 {
        h.clock.advance(TimeDelta::seconds(5));
        let began = Instant::now();
        h.tick().await;
        ticks.push(began.elapsed().as_millis());
    }
    eprintln!(
        "no-lease ticks: {ticks:?} ms, {} snapshot loads in 3 ticks",
        h.ctl.loads.load(std::sync::atomic::Ordering::Relaxed) - loads
    );
    let mut snaps = Vec::new();
    for _ in 0..3 {
        let began = Instant::now();
        let snap = h.store().work_monitor_snapshot(500, 0).await.unwrap();
        snaps.push(began.elapsed().as_millis());
        assert!(snap.items.len() >= 299);
    }
    eprintln!("monitor snapshot (500, 0): {snaps:?} ms");
}
