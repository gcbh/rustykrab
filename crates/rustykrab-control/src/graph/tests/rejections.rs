//! Every rejection reason of plan section 14.1 is reachable, and one filing
//! returns every failed check at once.

use std::collections::BTreeSet;

use rustykrab_core::work::{
    CancelReason, DraftEdge, EdgeKind, PlanOutcome, RejectionReason, Status, WorkKind,
};

use super::*;

/// P has a running planning child and one item in each state the rules
/// care about; Q sits outside it.
fn base() -> Snapshot {
    G::new()
        .add("P")
        .set("P", |p| p.budget = big())
        .status("P", Status::Running)
        .child("plan", "P")
        .status("plan", Status::Running)
        .child("done1", "P")
        .status("done1", Status::Done)
        .child("run1", "P")
        .status("run1", Status::Running)
        .child("wait1", "P")
        .child("fail1", "P")
        .status("fail1", Status::Failed)
        .child("canc1", "P")
        .status("canc1", Status::Cancelled(CancelReason::Requested))
        .add("Q")
        .snap()
}

fn planner() -> FilingContext {
    ctx(FilingSource::Planner)
}

fn with_edge(tmp_id: &str, kind: EdgeKind, up: ItemRef) -> WorkItemDraft {
    let mut d = draft(tmp_id);
    d.edges.push(DraftEdge {
        kind,
        depends_on: up,
    });
    d
}

fn superseding(tmp_id: &str, target: &str) -> WorkItemDraft {
    let mut d = draft(tmp_id);
    d.supersedes = Some(target.to_string());
    d
}

fn under_p(items: Vec<WorkItemDraft>) -> WorkPlan {
    plan(id("P"), items, vec![])
}

fn unknown_ref() -> Rejection {
    let a = with_edge("a", EdgeKind::Blocks, tmp("ghost"));
    reject(&base(), &under_p(vec![a]), &planner())
}

fn duplicate_tmp() -> Rejection {
    reject(&base(), &under_p(vec![draft("a"), draft("a")]), &planner())
}

fn invalid_item() -> Rejection {
    let mut a = draft("a");
    a.done_when = "  ".into();
    let mut b = draft("b");
    b.plan = true;
    let mut c = draft("c");
    c.budget = Some(Budget {
        iterations: 0,
        ..small()
    });
    reject(&base(), &under_p(vec![a, b, c]), &planner())
}

fn cycle() -> Rejection {
    let mut k = with_edge("k", EdgeKind::Blocks, tmp("G"));
    k.parent = Some(tmp("G"));
    let p = plan(
        id("P"),
        vec![draft("a"), draft("b"), draft("G"), k],
        vec![
            pe(tmp("a"), EdgeKind::Blocks, tmp("b")),
            pe(tmp("b"), EdgeKind::Blocks, tmp("a")),
        ],
    );
    reject(&base(), &p, &planner())
}

fn depth_exceeded() -> Rejection {
    let mut b = draft("b");
    b.parent = Some(tmp("a"));
    let mut c = planner();
    c.caps.max_depth = 2;
    reject(&base(), &under_p(vec![draft("a"), b]), &c)
}

fn too_many_items() -> Rejection {
    let mut c = planner();
    c.caps.max_items = 4;
    // plan, run1 and wait1 are open under P already.
    reject(&base(), &under_p(vec![draft("a"), draft("b")]), &c)
}

fn over_budget() -> Rejection {
    let mut a = draft("a");
    a.budget = Some(big());
    let mut c = planner();
    c.remaining_budget.insert("P".into(), small());
    reject(&base(), &under_p(vec![a]), &c)
}

fn single_writer_conflict() -> Rejection {
    let mut a = draft("a");
    a.writable_resources = vec!["calendar".into()];
    let mut b = draft("b");
    b.writable_resources = vec!["calendar".into()];
    reject(&base(), &under_p(vec![a, b]), &planner())
}

fn plan_b_edges() -> Rejection {
    let mut f = with_edge("f", EdgeKind::ConditionalOnFailure, id("wait1"));
    f.edges.push(DraftEdge {
        kind: EdgeKind::Blocks,
        depends_on: tmp("a"),
    });
    let f2 = with_edge("f2", EdgeKind::ConditionalOnFailure, id("wait1"));
    let x = with_edge("x", EdgeKind::Blocks, tmp("f2"));
    reject(&base(), &under_p(vec![draft("a"), f, f2, x]), &planner())
}

fn edge_onto_active() -> Rejection {
    let mut b = draft("b");
    b.parent = Some(id("run1"));
    let p = plan(
        id("P"),
        vec![draft("a"), b],
        vec![pe(id("run1"), EdgeKind::Blocks, tmp("a"))],
    );
    reject(&base(), &p, &planner())
}

fn input_unordered() -> Rejection {
    let mut a = draft("a");
    a.inputs_from = vec![tmp("b")];
    reject(&base(), &under_p(vec![a, draft("b")]), &planner())
}

fn dead_filing() -> Rejection {
    let held = with_edge("held", EdgeKind::Blocks, id("fail1"));
    let cancelled = with_edge("cancelled", EdgeKind::Blocks, id("canc1"));
    let no_plan_b = with_edge("no_plan_b", EdgeKind::ConditionalOnFailure, id("done1"));
    let mut orphan = draft("orphan");
    orphan.parent = Some(id("done1"));
    let mut stale = draft("stale");
    stale.expires_at = Some(t(-1));
    let behind = with_edge("behind", EdgeKind::Blocks, tmp("held"));
    reject(
        &base(),
        &under_p(vec![held, cancelled, no_plan_b, orphan, stale, behind]),
        &planner(),
    )
}

fn supersedes_active() -> Rejection {
    reject(
        &base(),
        &under_p(vec![superseding("a", "run1")]),
        &planner(),
    )
}

fn supersedes_closed() -> Rejection {
    reject(
        &base(),
        &under_p(vec![superseding("a", "done1")]),
        &planner(),
    )
}

fn out_of_scope() -> Rejection {
    let mut b = draft("b");
    b.parent = Some(id("Q"));
    reject(
        &base(),
        &under_p(vec![superseding("a", "Q"), b]),
        &planner(),
    )
}

fn kind_not_allowed() -> Rejection {
    let mut a = draft("a");
    a.kind = Some(WorkKind::Code);
    reject(&base(), &under_p(vec![a]), &planner())
}

fn already_planned() -> Rejection {
    let mut c = planner();
    c.already_planned = true;
    reject(&base(), &under_p(vec![draft("a")]), &c)
}

fn rate_limited() -> Rejection {
    let mut c = planner();
    c.supersedes_in_window = 1;
    c.supersede_limit = 1;
    reject(&base(), &under_p(vec![superseding("a", "wait1")]), &c)
}

fn only(r: &Rejection, reason: RejectionReason) -> Vec<Vec<ItemRef>> {
    assert_eq!(names(r), vec![reason.as_str()], "{r:#?}");
    r.failed.iter().map(|f| f.offending.clone()).collect()
}

#[test]
fn rejects_unknown_ref() {
    assert_eq!(
        only(&unknown_ref(), RejectionReason::UnknownRef),
        vec![vec![tmp("ghost")]]
    );
    // An existing id the caller may not see is unknown too.
    let mut c = planner();
    c.visible = Some(BTreeSet::from(["P".to_string(), "plan".to_string()]));
    let a = with_edge("a", EdgeKind::Blocks, id("done1"));
    let r = reject(&base(), &under_p(vec![a]), &c);
    assert_eq!(
        only(&r, RejectionReason::UnknownRef),
        vec![vec![id("done1")]]
    );
}

#[test]
fn rejects_duplicate_tmp() {
    assert_eq!(
        only(&duplicate_tmp(), RejectionReason::DuplicateTmp),
        vec![vec![tmp("a")]]
    );
}

#[test]
fn rejects_invalid_item() {
    let r = invalid_item();
    assert_eq!(
        only(&r, RejectionReason::InvalidItem),
        vec![vec![tmp("a")], vec![tmp("b")], vec![tmp("c")]]
    );
    assert_eq!(r.failed[0].detail, "missing done_when");
}

#[test]
fn rejects_cycle() {
    let offending = only(&cycle(), RejectionReason::Cycle);
    // k's edge onto its own parent, and the a/b loop.
    assert_eq!(
        offending,
        vec![vec![tmp("k"), tmp("G")], vec![tmp("a"), tmp("b")]]
    );
}

#[test]
fn rejects_depth_exceeded() {
    assert_eq!(
        only(&depth_exceeded(), RejectionReason::DepthExceeded),
        vec![vec![tmp("b")]]
    );
}

#[test]
fn rejects_too_many_items() {
    assert_eq!(
        only(&too_many_items(), RejectionReason::TooManyItems),
        vec![vec![id("P")]]
    );
}

#[test]
fn rejects_over_budget() {
    assert_eq!(
        only(&over_budget(), RejectionReason::OverBudget),
        vec![vec![tmp("a")]]
    );
}

#[test]
fn rejects_single_writer_conflict() {
    assert_eq!(
        only(
            &single_writer_conflict(),
            RejectionReason::SingleWriterConflict
        ),
        vec![vec![tmp("a"), tmp("b")]]
    );
    // Ordered writers are fine.
    let mut a = draft("a");
    a.writable_resources = vec!["calendar".into()];
    let mut b = with_edge("b", EdgeKind::WaitsFor, tmp("a"));
    b.writable_resources = vec!["calendar".into()];
    let mut s = base();
    accept(&mut s, &under_p(vec![a, b]), &planner());
}

#[test]
fn rejects_plan_b_edges() {
    assert_eq!(
        only(&plan_b_edges(), RejectionReason::PlanBEdges),
        vec![vec![tmp("f")], vec![tmp("x"), tmp("f2")]]
    );
}

#[test]
fn rejects_edge_onto_active() {
    assert_eq!(
        only(&edge_onto_active(), RejectionReason::EdgeOntoActive),
        vec![vec![id("run1")], vec![tmp("b"), id("run1")]]
    );
}

#[test]
fn rejects_input_unordered() {
    assert_eq!(
        only(&input_unordered(), RejectionReason::InputUnordered),
        vec![vec![tmp("a"), tmp("b")]]
    );
}

#[test]
fn rejects_dead_filing() {
    let r = dead_filing();
    let offending = only(&r, RejectionReason::DeadFiling);
    let mut named: Vec<ItemRef> = offending.into_iter().flatten().collect();
    named.sort_by_key(|r| format!("{r:?}"));
    let mut expected = vec![
        tmp("held"),
        tmp("cancelled"),
        tmp("no_plan_b"),
        tmp("orphan"),
        tmp("stale"),
        tmp("behind"),
    ];
    expected.sort_by_key(|r| format!("{r:?}"));
    assert_eq!(named, expected);
}

#[test]
fn rejects_a_new_child_under_a_parent_the_same_filing_supersedes() {
    let mut orphan = draft("orphan");
    orphan.parent = Some(id("wait1"));
    let r = reject(
        &base(),
        &under_p(vec![superseding("a", "wait1"), orphan]),
        &planner(),
    );
    assert_eq!(
        only(&r, RejectionReason::DeadFiling),
        vec![vec![tmp("orphan")]]
    );
}

#[test]
fn rejects_supersedes_active() {
    assert_eq!(
        only(&supersedes_active(), RejectionReason::SupersedesActive),
        vec![vec![id("run1")]]
    );
}

#[test]
fn rejects_supersedes_closed() {
    assert_eq!(
        only(&supersedes_closed(), RejectionReason::SupersedesClosed),
        vec![vec![id("done1")]]
    );
}

#[test]
fn rejects_out_of_scope() {
    assert_eq!(
        only(&out_of_scope(), RejectionReason::OutOfScope),
        vec![vec![tmp("b"), id("Q")], vec![id("Q")]]
    );
    // A root outside the caller's scope.
    let mut c = planner();
    c.scope = Some("wait1".into());
    let r = reject(&base(), &under_p(vec![draft("a")]), &c);
    assert_eq!(only(&r, RejectionReason::OutOfScope), vec![vec![id("P")]]);
}

#[test]
fn rejects_kind_not_allowed() {
    assert_eq!(
        only(&kind_not_allowed(), RejectionReason::KindNotAllowed),
        vec![vec![tmp("a")]]
    );
}

#[test]
fn rejects_already_planned() {
    assert_eq!(
        only(&already_planned(), RejectionReason::AlreadyPlanned),
        vec![vec![id("P")]]
    );
}

#[test]
fn rejects_rate_limited() {
    assert_eq!(
        only(&rate_limited(), RejectionReason::RateLimited),
        vec![vec![id("wait1")]]
    );
}

#[test]
fn every_rejection_reason_is_reachable() {
    let all = [
        unknown_ref(),
        duplicate_tmp(),
        invalid_item(),
        cycle(),
        depth_exceeded(),
        too_many_items(),
        over_budget(),
        single_writer_conflict(),
        plan_b_edges(),
        edge_onto_active(),
        input_unordered(),
        dead_filing(),
        supersedes_active(),
        supersedes_closed(),
        out_of_scope(),
        kind_not_allowed(),
        already_planned(),
        rate_limited(),
    ];
    let reached: BTreeSet<&str> = all.iter().flat_map(names).collect();
    let expected: BTreeSet<&str> = [
        RejectionReason::UnknownRef,
        RejectionReason::DuplicateTmp,
        RejectionReason::InvalidItem,
        RejectionReason::Cycle,
        RejectionReason::DepthExceeded,
        RejectionReason::TooManyItems,
        RejectionReason::OverBudget,
        RejectionReason::SingleWriterConflict,
        RejectionReason::PlanBEdges,
        RejectionReason::EdgeOntoActive,
        RejectionReason::InputUnordered,
        RejectionReason::DeadFiling,
        RejectionReason::SupersedesActive,
        RejectionReason::SupersedesClosed,
        RejectionReason::OutOfScope,
        RejectionReason::KindNotAllowed,
        RejectionReason::AlreadyPlanned,
        RejectionReason::RateLimited,
    ]
    .iter()
    .map(|r| r.as_str())
    .collect();
    assert_eq!(reached, expected);
}

#[test]
fn validation_returns_every_failed_check_at_once() {
    let mut blank = draft("x");
    blank.title = String::new();
    let mut code = draft("k");
    code.kind = Some(WorkKind::Code);
    let ghost = with_edge("u", EdgeKind::Blocks, tmp("ghost"));
    let mut w1 = draft("w1");
    w1.writable_resources = vec!["mailbox".into()];
    let mut w2 = draft("w2");
    w2.writable_resources = vec!["mailbox".into()];
    let mut reader = draft("i");
    reader.inputs_from = vec![tmp("w1")];
    let p = plan(
        id("P"),
        vec![
            blank,
            draft("x"),
            code,
            ghost,
            draft("c1"),
            draft("c2"),
            w1,
            w2,
            reader,
        ],
        vec![
            pe(tmp("c1"), EdgeKind::Blocks, tmp("c2")),
            pe(tmp("c2"), EdgeKind::Blocks, tmp("c1")),
        ],
    );
    let s = base();
    let r = reject(&s, &p, &planner());
    assert_eq!(
        names(&r),
        vec![
            "duplicate_tmp",
            "unknown_ref",
            "invalid_item",
            "kind_not_allowed",
            "cycle",
            "input_unordered",
            "single_writer_conflict",
        ]
    );
    // The tool result carries the same list; nothing was filed.
    match validate(&s, &p, &planner()).to_outcome() {
        PlanOutcome::Rejected(core) => assert_eq!(core.failed.len(), r.failed.len()),
        PlanOutcome::Accepted(_) => panic!("accepted"),
    }
    assert_eq!(s.items().len(), base().items().len());
}
