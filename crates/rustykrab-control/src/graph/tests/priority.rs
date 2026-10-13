use super::*;
use crate::graph::scheduling_priorities;

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
