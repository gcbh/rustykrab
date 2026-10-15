use super::*;
use crate::graph::scheduling_priorities;
use rustykrab_core::work::BlockedReason;

#[test]
fn explicit_urgency_precedes_blocker_impact_and_inactive_plan_b_donates_nothing() {
    let g = G::new()
        .add("repair")
        .add("first")
        .add("second")
        .add("urgent")
        .add("alternative")
        .add("attempt")
        .set("urgent", |i| i.priority = 999)
        .set("alternative", |i| i.priority = 2000)
        .edge("first", EdgeKind::Blocks, "repair")
        .edge("second", EdgeKind::Blocks, "repair")
        .edge("alternative", EdgeKind::Blocks, "repair")
        .edge("alternative", EdgeKind::ConditionalOnFailure, "attempt");
    let ranks = scheduling_priorities(&g.snap(), now());
    assert_eq!(ranks["repair"].waiters, 2);
    assert_eq!(ranks["repair"].priority, 0);
    assert!(ranks["repair"].compare(&ranks["urgent"]).is_gt());
    let failed = g.status("attempt", Status::Failed);
    assert_eq!(
        scheduling_priorities(&failed.snap(), now())["repair"].waiters,
        3
    );
    let parent = G::new()
        .add("project")
        .child("fallback", "project")
        .add("attempt")
        .add("repair")
        .edge("fallback", EdgeKind::ConditionalOnFailure, "attempt")
        .edge("fallback", EdgeKind::Blocks, "repair");
    assert_eq!(
        scheduling_priorities(&parent.snap(), now())["repair"].waiters,
        0,
        "a parent does not donate through an unreleased fallback"
    );
}

#[test]
fn one_priority_list_retains_blocked_leaves_and_excludes_goals_and_history() {
    let g = G::new()
        .add("goal")
        .child("work", "goal")
        .add("repair")
        .add("held")
        .add("closed")
        .set("work", |i| i.priority = 20)
        .set("work", |i| {
            i.objective = "Restore reliable self-improvement".into();
            i.done_when = "A scheduled task completes with verified output".into();
        })
        .set("held", |i| {
            i.priority = 30;
            i.held_by = Some("consent".into());
            i.status = Status::Blocked(BlockedReason::NeedsConsent);
        })
        .status("closed", Status::Done)
        .edge("work", EdgeKind::Blocks, "repair");
    let list = crate::graph::priority_list(&g.snap(), now());
    assert_eq!(
        list.entries
            .iter()
            .map(|e| e.item.as_str())
            .collect::<Vec<_>>(),
        vec!["held", "repair", "work"]
    );
    assert_eq!(list.entries[0].wait_reason, "Waiting for your approval");
    assert_eq!(list.entries[1].effective_priority, 20);
    assert_eq!(list.entries[2].waiting_on[0].item, "repair");
    assert_eq!(list.entries[2].goal.as_ref().unwrap().item, "goal");
    assert_eq!(
        list.entries[2].objective,
        "Restore reliable self-improvement"
    );
    assert_eq!(
        list.entries[2].done_when,
        "A scheduled task completes with verified output"
    );
    assert!(!list.entries[2].description_truncated);
    assert_eq!(
        list.entries.iter().map(|e| e.position).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(list, crate::graph::priority_list(&g.snap(), now()));
    let changed = g.set("held", |i| i.priority = 1);
    assert_ne!(
        list.revision,
        crate::graph::priority_list(&changed.snap(), now()).revision
    );
}

#[test]
fn priority_descriptions_are_bounded_unicode_previews_without_changing_the_task() {
    let g = G::new().add("work").set("work", |item| {
        item.objective = "修".repeat(801);
        item.done_when = "✅".repeat(401);
    });
    let snap = g.snap();
    let list = crate::graph::priority_list(&snap, now());
    assert_eq!(list.entries[0].objective.chars().count(), 800);
    assert_eq!(list.entries[0].done_when.chars().count(), 400);
    assert!(list.entries[0].description_truncated);
    assert_eq!(snap.item("work").unwrap().objective.chars().count(), 801);
    assert_eq!(snap.item("work").unwrap().done_when.chars().count(), 401);
}

#[test]
fn urgency_reaches_transitive_prerequisites_and_disappears_with_the_waiter() {
    let g = G::new()
        .add("urgent")
        .add("middle")
        .add("repair")
        .add("routine")
        .set("urgent", |i| i.priority = 10)
        .set("routine", |i| i.priority = 5)
        .edge("urgent", EdgeKind::Blocks, "middle")
        .edge("middle", EdgeKind::Blocks, "repair");
    let ranks = scheduling_priorities(&g.snap(), now());
    assert_eq!(ranks["repair"].priority, 10);
    assert_eq!(ranks["repair"].waiters, 2);
    assert!(ranks["repair"].priority > ranks["routine"].priority);
    assert_eq!(g.snap().item("repair").unwrap().priority, 0);
    let closed = g.status("urgent", Status::Done);
    assert_eq!(
        scheduling_priorities(&closed.snap(), now())["repair"].priority,
        0
    );
}

#[test]
fn shared_blockers_count_distinct_waiters_even_through_diamonds() {
    let g = G::new()
        .add("top")
        .add("left")
        .add("right")
        .add("repair")
        .edge("top", EdgeKind::Blocks, "left")
        .edge("top", EdgeKind::Blocks, "right")
        .edge("left", EdgeKind::Blocks, "repair")
        .edge("right", EdgeKind::Blocks, "repair");
    assert_eq!(scheduling_priorities(&g.snap(), now())["repair"].waiters, 3);
}

#[test]
fn parent_urgency_reaches_children_and_their_inherited_prerequisites() {
    let g = G::new()
        .add("project")
        .child("slice", "project")
        .add("repair")
        .set("project", |i| i.priority = 20)
        .edge("project", EdgeKind::Blocks, "repair");
    let ranks = scheduling_priorities(&g.snap(), now());
    assert_eq!(ranks["slice"].priority, 20);
    assert_eq!(ranks["repair"].priority, 20);
}

#[test]
fn inactive_work_and_history_do_not_raise_priority_and_cycles_terminate() {
    let g = G::new()
        .add("held")
        .add("future")
        .add("expired")
        .add("closed")
        .add("history")
        .add("alternative")
        .add("repair")
        .set("held", |i| {
            i.priority = 99;
            i.held_by = Some("approval".into());
        })
        .set("future", |i| {
            i.priority = 99;
            i.trigger = Trigger::At(t(1));
        })
        .set("expired", |i| {
            i.priority = 99;
            i.expires_at = Some(t(-1));
        })
        .set("closed", |i| {
            i.priority = 99;
            i.status = Status::Done;
        })
        .set("history", |i| i.priority = 99)
        .set("alternative", |i| i.priority = 99)
        .edge("held", EdgeKind::Blocks, "repair")
        .edge("future", EdgeKind::Blocks, "repair")
        .edge("expired", EdgeKind::Blocks, "repair")
        .edge("closed", EdgeKind::Blocks, "repair")
        .edge("history", EdgeKind::DiscoveredFrom, "repair")
        .edge("alternative", EdgeKind::ConditionalOnFailure, "repair");
    let ranks = scheduling_priorities(&g.snap(), now());
    assert_eq!(ranks["repair"].priority, 0);
    assert_eq!(ranks["repair"].waiters, 0);
    let cyclic = G::new()
        .add("a")
        .add("b")
        .edge("a", EdgeKind::Blocks, "b")
        .edge("b", EdgeKind::Blocks, "a");
    assert_eq!(scheduling_priorities(&cyclic.snap(), now())["a"].waiters, 1);
}
