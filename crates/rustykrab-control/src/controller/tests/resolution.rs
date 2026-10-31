use super::*;
use crate::controller::batch::Batch;
use rustykrab_core::work::{BlockedReason, ErrorClass, WorkKind};

#[tokio::test]
async fn shared_blockers_run_before_independent_work_at_equal_priority() {
    let h = Harness::new(&["agent"]);
    let repair = h.file_one(draft("repair", "shared prerequisite")).await;
    for title in ["blocked first", "blocked second"] {
        let mut d = draft(title, title);
        d.edges.push(DraftEdge {
            kind: EdgeKind::Blocks,
            depends_on: ItemRef::Id(repair.clone()),
        });
        h.file_one(d).await;
    }
    let mut urgent = draft("urgent", "independent urgent work");
    urgent.priority = 0;
    h.file_one(urgent).await;
    let gate = Arc::new(Notify::new());
    h.script.push(
        "shared prerequisite",
        Step::Wait(gate.clone(), Box::new(done("prerequisite"))),
    );
    assert_eq!(h.tick().await.leased, vec![repair.clone()]);
    let ranks = h.ctl.queue_priorities().await.unwrap();
    assert_eq!(ranks[&repair].waiters, 2);
    assert!(h.script.briefs_for("independent urgent work").is_empty());
    gate.notify_one();
    h.drain().await;
}

#[tokio::test]
async fn a_failure_creates_a_prerequisite_and_schedules_it_before_routine_work() {
    let h = Harness::new(&["worker"]);
    h.script.push(
        "urgent task",
        Step::Fail(Error::Internal("flux capacitor desynchronised".into())),
    );
    let mut urgent = draft("urgent", "urgent task");
    urgent.priority = 10;
    let urgent = h.file_one(urgent).await;
    let mut routine = draft("routine", "routine task");
    routine.priority = 5;
    let routine = h.file_one(routine).await;
    h.tick().await;
    h.ctl.set_draining(true);
    h.step().await;
    let repairs = h.of_kind(WorkKind::Internal).await;
    assert_eq!(repairs.len(), 1);
    let repair = &repairs[0];
    // The prerequisite lands something the controller checks: a classifier
    // rule that classifies the unknown failure on replay. One that landed
    // nothing would park for its owner instead (`tests/investigation.rs`).
    let mut landed = done("repair");
    landed.artifacts.push(ArtifactRef {
        kind: crate::errors::CLASSIFIER_RULE.into(),
        value: "process: flux capacitor".into(),
    });
    let gate = Arc::new(Notify::new());
    h.script
        .push(&repair.title, Step::Wait(gate.clone(), Box::new(landed)));
    h.ctl.set_draining(false);
    assert_eq!(h.tick().await.leased, vec![repair.id.clone()]);
    assert_eq!(h.script.briefs_for("urgent task").len(), 1);
    assert!(h.script.briefs_for("routine task").is_empty());
    assert_eq!(h.status(&urgent).await, Status::Queued);
    assert_eq!(
        h.store()
            .work_get(&repair.id)
            .await
            .unwrap()
            .unwrap()
            .priority,
        0
    );
    gate.notify_one();
    h.drain().await;
    assert_eq!(h.status(&urgent).await, Status::Done);
    assert_eq!(h.status(&routine).await, Status::Done);
    let brief = h.script.briefs_for("urgent task").pop().unwrap();
    assert!(brief.inputs.iter().any(|i| i.item == repair.id));
    assert!(brief.last_error.is_some());
}

#[tokio::test]
async fn concurrent_failures_share_one_durable_prerequisite_across_restart() {
    let h = Harness::new(&["one", "two"]);
    for title in ["first task", "second task"] {
        h.script.push(
            title,
            Step::Fail(Error::Internal("flux capacitor desynchronised".into())),
        );
    }
    let first = h.file_one(draft("first", "first task")).await;
    let second = h.file_one(draft("second", "second task")).await;
    h.tick().await;
    h.ctl.set_draining(true);
    h.step().await;
    let repairs = h.of_kind(WorkKind::Internal).await;
    assert_eq!(repairs.len(), 1);
    let repair = &repairs[0];
    let snap = h.ctl.load().await.unwrap();
    for waiter in [&first, &second] {
        assert!(snap
            .edges_held_by(waiter)
            .any(|e| e.kind == EdgeKind::Blocks && e.depends_on == repair.id));
        assert!(repair
            .artifact_refs
            .iter()
            .any(|r| r.kind == "item" && &r.value == waiter));
    }
    let h = h.restart(&["one", "two"]);
    h.drain().await;
    assert_eq!(h.status(&first).await, Status::Done);
    assert_eq!(h.status(&second).await, Status::Done);
    assert_eq!(h.script.briefs_for(&repair.title).len(), 1);
}

#[tokio::test]
async fn a_resolution_cannot_become_its_own_prerequisite() {
    let h = Harness::new(&[]);
    let error = WorkError {
        class: ErrorClass::Unknown,
        subclass: ErrorSubclass::Unclassified,
        fingerprint: "test-fingerprint".into(),
        detail: "unclassified failure".into(),
        observed_by: "test".into(),
        artifact_refs: vec![],
    };
    let resolution = h
        .file_one(crate::errors::internal_item_draft(&error, vec![]))
        .await;
    let mut batch = Batch::new(h.ctl.load().await.unwrap(), h.clock.now());
    let item = batch.snap.item(&resolution).unwrap().clone();
    let why = h
        .ctl
        .file_internal(&mut batch, &item, &error, None)
        .unwrap_err();
    assert!(why.contains("cycle"), "{why}");
    assert!(batch.ops.is_empty());
}

#[tokio::test]
async fn an_existing_resolution_is_reused_by_identity_across_work_kinds_and_titles() {
    let h = Harness::new(&["worker"]);
    let error = WorkError {
        class: ErrorClass::Unknown,
        subclass: ErrorSubclass::Unclassified,
        fingerprint: "test-fingerprint".into(),
        detail: "unclassified failure".into(),
        observed_by: "test".into(),
        artifact_refs: vec![],
    };
    let mut known = crate::errors::internal_item_draft(&error, vec![]);
    known.title = "Existing investigation of the root cause".into();
    known.kind = Some(WorkKind::Research);
    let known = h.file_one(known).await;
    let waiter = h.file_one(draft("waiter", "waiting work")).await;
    let mut batch = Batch::new(h.ctl.load().await.unwrap(), h.clock.now());
    let item = batch.snap.item(&waiter).unwrap().clone();
    assert_eq!(
        h.ctl
            .file_internal(&mut batch, &item, &error, None)
            .unwrap(),
        known
    );
    assert_eq!(batch.snap.items().len(), 2);
    assert!(batch
        .snap
        .edges_held_by(&waiter)
        .any(|e| e.depends_on == known));
    assert_eq!(batch.snap.item(&known).unwrap().kind, WorkKind::Research);
    h.ctl.commit(batch, &mut Default::default()).await.unwrap();
    h.drain().await;
    assert_eq!(h.status(&waiter).await, Status::Done);
    assert!(h.script.briefs_for("waiting work")[0]
        .inputs
        .iter()
        .any(|i| i.item == known));
}

#[tokio::test]
async fn failing_diagnoses_cannot_spawn_an_unbounded_resolution_chain() {
    let h = Harness::new(&["worker"]);
    let mut diagnosis = draft("diagnosis", "existing diagnosis");
    diagnosis.kind = Some(WorkKind::Internal);
    diagnosis.artifact_refs.push(ArtifactRef {
        kind: "resolution_depth".into(),
        value: h.config.caps.max_depth.to_string(),
    });
    h.script.push(
        &diagnosis.title,
        Step::Fail(Error::Internal("new unexplained diagnosis failure".into())),
    );
    let diagnosis = h.file_one(diagnosis).await;
    h.drain().await;
    assert_eq!(h.of_kind(WorkKind::Internal).await.len(), 1);
    assert_eq!(h.status(&diagnosis).await, Status::Done);
    assert_eq!(h.rungs(&diagnosis).await, vec![Rung::Improve, Rung::Repair]);
    assert!(h.events(&diagnosis).await.iter().any(|e| e
        .reason
        .as_deref()
        .is_some_and(|r| r.contains("depth limit reached"))));
}

#[tokio::test]
async fn restart_recovers_only_controller_resolution_waits_on_a_stopped_prerequisite() {
    use rustykrab_store::{TransitionSpec, WorkOp};
    let h = Harness::new(&["worker"]);
    h.script.push(
        "waiting work",
        Step::Fail(Error::Internal("flux capacitor desynchronised".into())),
    );
    let waiter = h.file_one(draft("waiter", "waiting work")).await;
    h.tick().await;
    h.ctl.set_draining(true);
    h.step().await;
    let fix = h.of_kind(WorkKind::Internal).await.pop().unwrap();
    let mut other = draft("ordinary", "ordinary dependency");
    other.edges.push(DraftEdge {
        kind: EdgeKind::Blocks,
        depends_on: ItemRef::Id(fix.id.clone()),
    });
    let other = h.file_one(other).await;
    let mut parked = TransitionSpec::new(
        &fix.id,
        Status::Blocked(BlockedReason::NeedsDecision),
        "controller",
    );
    parked.reason = Some("repair failed".into());
    h.store()
        .work_apply(vec![WorkOp::Transition(parked)])
        .await
        .unwrap();
    let h = h.restart(&[]);
    h.ctl.set_draining(true);
    h.tick().await;
    assert_eq!(h.status(&waiter).await, Status::Ready);
    assert_eq!(h.status(&other).await, Status::Queued);
    assert_eq!(
        h.status(&fix.id).await,
        Status::Blocked(BlockedReason::NeedsDecision)
    );
    let snap = h.ctl.load().await.unwrap();
    assert!(!snap
        .edges_held_by(&waiter)
        .any(|e| e.kind == EdgeKind::Blocks));
    assert!(snap.edges_held_by(&other).any(|e| e.depends_on == fix.id));
    h.tick().await;
    assert_eq!(
        h.events(&waiter)
            .await
            .iter()
            .filter(|e| e.kind == EventKind::Resume)
            .count(),
        1
    );
}
