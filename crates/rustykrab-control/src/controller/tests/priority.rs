use super::*;
use rustykrab_core::work::BlockedReason;

#[tokio::test]
async fn displayed_priority_order_persists_and_drives_the_next_dispatch() {
    let h = Harness::new(&["worker"]);
    let first = h.file_one(draft("first", "first")).await;
    let second = h.file_one(draft("second", "second")).await;
    let before = h.ctl.priority_list().await.unwrap().unwrap();
    assert_eq!(before.entries[0].item, first);
    let reordered = h
        .ctl
        .reorder_priority(
            &before.revision,
            &[second.clone(), first.clone()],
            "user:test",
        )
        .await
        .unwrap();
    assert_eq!(reordered.entries[0].item, second);
    let h = h.restart(&["worker"]);
    assert_eq!(h.ctl.priority_list().await.unwrap().unwrap(), reordered);
    let gate = Arc::new(Notify::new());
    h.script
        .push("second", Step::Wait(gate.clone(), Box::new(done("second"))));
    assert_eq!(h.tick().await.leased, vec![second.clone()]);
    assert!(h.script.briefs_for("first").is_empty());
    gate.notify_one();
    h.drain().await;
}

#[tokio::test]
async fn a_blocked_first_item_keeps_its_position_while_the_next_eligible_item_runs() {
    let h = Harness::new(&["worker"]);
    let mut blocked = draft("blocked", "blocked");
    blocked.priority = 100;
    let blocked = h.file_one(blocked).await;
    h.store()
        .work_apply(vec![rustykrab_store::WorkOp::Transition(
            rustykrab_store::TransitionSpec::new(
                &blocked,
                Status::Blocked(BlockedReason::NeedsDecision),
                "controller",
            ),
        )])
        .await
        .unwrap();
    let next = h.file_one(draft("next", "next")).await;
    let before = h.ctl.priority_list().await.unwrap().unwrap();
    assert_eq!(before.entries[0].item, blocked);
    let gate = Arc::new(Notify::new());
    h.script
        .push("next", Step::Wait(gate.clone(), Box::new(done("next"))));
    assert_eq!(h.tick().await.leased, vec![next]);
    assert_eq!(
        h.ctl.priority_list().await.unwrap().unwrap().entries[0].item,
        blocked
    );
    assert_eq!(
        h.status(&blocked).await,
        Status::Blocked(BlockedReason::NeedsDecision)
    );
    gate.notify_one();
    h.drain().await;
}

#[tokio::test]
async fn stale_partial_duplicate_and_nonhuman_reorders_change_nothing() {
    let h = Harness::new(&[]);
    let first = h.file_one(draft("first", "first")).await;
    let second = h.file_one(draft("second", "second")).await;
    let before = h.ctl.priority_list().await.unwrap().unwrap();
    for (revision, order, actor) in [
        ("stale", vec![second.clone(), first.clone()], "user:test"),
        (before.revision.as_str(), vec![second.clone()], "user:test"),
        (
            before.revision.as_str(),
            vec![second.clone(), second.clone()],
            "user:test",
        ),
        (
            before.revision.as_str(),
            vec![second.clone(), first.clone()],
            "worker:agent",
        ),
    ] {
        assert!(h
            .ctl
            .reorder_priority(revision, &order, actor)
            .await
            .is_err());
        assert_eq!(h.ctl.priority_list().await.unwrap().unwrap(), before);
    }
}

#[tokio::test]
async fn prerequisites_cannot_be_moved_below_the_work_they_gate() {
    let h = Harness::new(&[]);
    let dependency = h.file_one(draft("dependency", "dependency")).await;
    let mut waiter = draft("waiter", "waiter");
    waiter.edges.push(DraftEdge {
        kind: EdgeKind::Blocks,
        depends_on: ItemRef::Id(dependency.clone()),
    });
    let waiter = h.file_one(waiter).await;
    let before = h.ctl.priority_list().await.unwrap().unwrap();
    assert!(h
        .ctl
        .reorder_priority(&before.revision, &[waiter, dependency], "user:test")
        .await
        .unwrap_err()
        .to_string()
        .contains("Prerequisite"));
    assert_eq!(h.ctl.priority_list().await.unwrap().unwrap(), before);
}

#[tokio::test]
async fn reordering_preserves_holds_scope_budget_and_the_hold_timestamp() {
    let h = Harness::new(&[]);
    let first = h.file_one(draft("first", "first")).await;
    let second = h.file_one(draft("second", "second")).await;
    h.store()
        .work_apply(vec![rustykrab_store::WorkOp::Transition(
            rustykrab_store::TransitionSpec::new(
                &first,
                Status::Blocked(BlockedReason::BudgetExhausted),
                "controller",
            ),
        )])
        .await
        .unwrap();
    let original = h.store().work_get(&first).await.unwrap().unwrap();
    let before = h.ctl.priority_list().await.unwrap().unwrap();
    let order = if before.entries[0].item == first {
        vec![second, first.clone()]
    } else {
        vec![first.clone(), second]
    };
    h.ctl
        .reorder_priority(&before.revision, &order, "user:test")
        .await
        .unwrap();
    let mut expected = original;
    let actual = h.store().work_get(&first).await.unwrap().unwrap();
    expected.priority = actual.priority;
    assert_eq!(actual, expected);
    assert!(h
        .store()
        .work_events(&first)
        .await
        .unwrap()
        .iter()
        .any(|e| e.kind == EventKind::Decision && e.actor == "user:test"));
}
