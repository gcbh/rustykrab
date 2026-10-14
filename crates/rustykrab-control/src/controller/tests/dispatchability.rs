use super::*;
use crate::{handle::LockState, lock::LoopLock};

async fn warm(h: &Harness) -> LoopLock {
    let mut lock = LoopLock::in_data_dir(&h.dir.0);
    assert_eq!(h.ctl.claim_loop_lock(&mut lock).0, LockState::Held);
    h.tick().await;
    lock
}

#[tokio::test]
async fn demand_uses_real_matcher_without_leasing_or_changing_work() {
    let h = Harness::new(&["pinch"]);
    let _lock = warm(&h).await;
    let mut item = draft("target", "Useful foreground");
    item.required_tools = vec!["exec".into()];
    h.file(plan(vec![item])).await;
    let rows = h.store.work_list(&Default::default()).await.unwrap();
    let cursor = h.store.work_events_last_id().await.unwrap();
    let demand = h.ctl.foreground_dispatchability().await.unwrap().unwrap();
    assert_eq!(demand.dispatchable_foreground_leaves, 1);
    assert_eq!(demand.active_foreground_runs, 0);
    assert_eq!(h.store.work_list(&Default::default()).await.unwrap(), rows);
    assert_eq!(h.store.work_events_last_id().await.unwrap(), cursor);
    assert!(h.script.briefs().is_empty());
}

#[tokio::test]
async fn unexecutable_backlog_distinguishes_dependencies_holds_and_missing_coverage() {
    let h = Harness::new(&["pinch"]);
    let _lock = warm(&h).await;
    let mut first = draft("up", "Unavailable resource");
    first.required_tools = vec!["not-in-worker-ceiling".into()];
    let up = h.file(plan(vec![first])).await;
    let down = h
        .file(plan(vec![draft("down", "Wait for unavailable resource")]))
        .await;
    h.store
        .work_add_edges(&[rustykrab_core::work::Edge {
            item: down.root,
            depends_on: up.root,
            kind: EdgeKind::Blocks,
        }])
        .await
        .unwrap();
    let demand = h.ctl.foreground_dispatchability().await.unwrap().unwrap();
    assert_eq!(demand.dispatchable_foreground_leaves, 0);
    assert_eq!(demand.waiting["matcher_unavailable"], 1);
    assert_eq!(demand.waiting["dependencies"], 1);
    assert!(h.script.briefs().is_empty());
}

#[tokio::test]
async fn spent_leaf_budget_is_waiting_not_dispatchable_and_is_not_replenished() {
    let h = Harness::new(&["pinch"]);
    let _lock = warm(&h).await;
    let accepted = h
        .file(plan(vec![draft("budget", "Spent foreground")]))
        .await;
    h.store
        .work_spend_record(rustykrab_store::RunSpend {
            item: accepted.root.clone(),
            run: Some("prior-run".into()),
            worker: "pinch".into(),
            tokens: 200_000,
            wall_ms: 0,
            iterations: 0,
            at: Utc::now(),
        })
        .await
        .unwrap();
    // Restart the memoized accounting the same way a fresh controller load does.
    h.ctl.state().spent = None;
    let before = h.store.work_get(&accepted.root).await.unwrap();
    let demand = h.ctl.foreground_dispatchability().await.unwrap().unwrap();
    assert_eq!(demand.dispatchable_foreground_leaves, 0);
    assert_eq!(demand.waiting["cumulative_budget"], 1);
    assert_eq!(h.store.work_get(&accepted.root).await.unwrap(), before);
    assert!(h.script.briefs().is_empty());
}

#[tokio::test]
async fn project_reviews_are_excluded_but_active_foreground_keeps_demand() {
    let h = Harness::new(&["pinch"]);
    let _lock = warm(&h).await;
    let mut review = draft("review", "Project review");
    review.kind = Some(rustykrab_core::work::WorkKind::Research);
    review.artifact_refs = vec![
        ArtifactRef {
            kind: "dream_review_job".into(),
            value: "cycle:generator".into(),
        },
        ArtifactRef {
            kind: rustykrab_core::dream_review::REVIEW_ONLY.into(),
            value: "true".into(),
        },
    ];
    let accepted = h.file(plan(vec![review])).await;
    assert_eq!(
        h.ctl
            .foreground_dispatchability()
            .await
            .unwrap()
            .unwrap()
            .dispatchable_foreground_leaves,
        0
    );
    h.ctl
        .cancel(&accepted.root, Some("fixture cleanup".into()), "user")
        .await
        .unwrap();
    h.script.push(
        "Foreground in flight",
        Step::Hang(Arc::new(AtomicBool::new(false))),
    );
    let active = h
        .file(plan(vec![draft("active", "Foreground in flight")]))
        .await;
    h.tick().await;
    let demand = h.ctl.foreground_dispatchability().await.unwrap().unwrap();
    assert_eq!(demand.active_foreground_runs, 1);
    h.ctl
        .cancel(&active.root, Some("fixture cleanup".into()), "user")
        .await
        .unwrap();
}

#[tokio::test]
async fn absent_stale_draining_or_busy_loop_is_unknown_not_idle() {
    let h = Harness::new(&["pinch"]);
    assert!(h.ctl.foreground_dispatchability().await.unwrap().is_none());
    let _lock = warm(&h).await;
    let busy = h.ctl.loop_lock.lock().await;
    assert!(h.ctl.foreground_dispatchability().await.unwrap().is_none());
    drop(busy);
    h.ctl.set_draining(true);
    assert!(h.ctl.foreground_dispatchability().await.unwrap().is_none());
    h.ctl.set_draining(false);
    h.clock.advance(TimeDelta::seconds(61));
    assert!(h.ctl.foreground_dispatchability().await.unwrap().is_none());
}

#[tokio::test]
async fn owner_holds_and_external_model_activity_do_not_become_dispatch_capacity() {
    struct Busy;
    impl super::super::ModelActivity for Busy {
        fn busy(&self, _: &str) -> bool {
            true
        }
    }
    let mut h = Harness::new(&["pinch"]);
    h.ctl = h.ctl.with_activity(Arc::new(Busy));
    let _lock = warm(&h).await;
    let accepted = h
        .file(plan(vec![draft(
            "q",
            "Queued while interactive model is busy",
        )]))
        .await;
    let mut held = h.store.work_get(&accepted.root).await.unwrap().unwrap();
    held.id = "independently-held".into();
    held.held_by = Some("owner-question".into());
    h.store
        .work_insert_graph(std::slice::from_ref(&held), &[], None)
        .await
        .unwrap();
    let demand = h.ctl.foreground_dispatchability().await.unwrap().unwrap();
    assert_eq!(demand.dispatchable_foreground_leaves, 0);
    assert_eq!(demand.waiting["matcher_unavailable"], 1);
    assert_eq!(demand.waiting["owner_hold"], 1);
    assert_eq!(
        h.store.work_get(&held.id).await.unwrap().unwrap().held_by,
        held.held_by
    );
}

#[tokio::test]
async fn single_writer_resources_remain_busy_even_when_worker_has_another_slot() {
    let mut h = Harness::with_concurrency(&["pinch"], 2);
    h.ctl.config.serialise_local = false;
    let _lock = warm(&h).await;
    let mut first = draft("first", "Shared writer in flight");
    first.writable_resources = vec!["calendar:fixture".into()];
    h.script.push(
        "Shared writer in flight",
        Step::Hang(Arc::new(AtomicBool::new(false))),
    );
    let first = h.file(plan(vec![first])).await;
    h.ctl.approve(&first.root, "user").await.unwrap();
    h.tick().await;
    let mut next = draft("next", "Second shared writer");
    next.writable_resources = vec!["calendar:fixture".into()];
    let next = h.file(plan(vec![next])).await;
    h.ctl.approve(&next.root, "user").await.unwrap();
    let demand = h.ctl.foreground_dispatchability().await.unwrap().unwrap();
    assert_eq!(demand.active_foreground_runs, 1);
    assert_eq!(demand.dispatchable_foreground_leaves, 0);
    assert_eq!(demand.waiting["matcher_unavailable"], 1);
    h.ctl
        .cancel(&first.root, Some("fixture cleanup".into()), "user")
        .await
        .unwrap();
}

#[tokio::test]
async fn tool_routing_history_requires_real_worker_coverage() {
    let h = Harness::new(&["pinch"]);
    let _lock = warm(&h).await;
    let accepted = h
        .file(plan(vec![draft("routed", "Routed host tool")]))
        .await;
    let rung = rustykrab_core::work::RungEvent {
        rung: Rung::SwitchWorker,
        at: h.clock.now(),
        error: None,
        outcome: format!(
            "{}native{}unavailable-tool",
            super::super::load::SWITCHED,
            super::super::load::ROUTED_FOR
        ),
    };
    h.store
        .work_event_append(&WorkEvent {
            item: accepted.root,
            at: h.clock.now(),
            kind: EventKind::Rung,
            from: None,
            to: None,
            actor: "controller".into(),
            reason: Some(crate::ladder::encode_rung_event(&rung)),
            upstream: None,
            origin: None,
            evidence_ref: None,
        })
        .await
        .unwrap();
    let demand = h.ctl.foreground_dispatchability().await.unwrap().unwrap();
    assert_eq!(demand.dispatchable_foreground_leaves, 0);
    assert_eq!(demand.waiting["matcher_unavailable"], 1);
    assert!(h.script.briefs().is_empty());
}

#[tokio::test]
async fn in_flight_probe_occupies_an_otherwise_idle_worker_slot() {
    let h = Harness::new(&["pinch"]);
    let _lock = warm(&h).await;
    h.file(plan(vec![draft("waiting", "Waiting during a probe")]))
        .await;
    h.ctl.state().probing.insert("pinch".into(), 1);
    let demand = h.ctl.foreground_dispatchability().await.unwrap().unwrap();
    assert_eq!(demand.dispatchable_foreground_leaves, 0);
    assert_eq!(demand.waiting["matcher_unavailable"], 1);
    h.ctl.state().probing.clear();
    assert_eq!(
        h.ctl
            .foreground_dispatchability()
            .await
            .unwrap()
            .unwrap()
            .dispatchable_foreground_leaves,
        1
    );
    assert!(h.script.briefs().is_empty());
}
