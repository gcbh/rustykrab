//! Plan section 15, scenarios 18 to 26 and 28: the parts that are pure
//! graph behaviour.

use std::collections::HashMap;

use rustykrab_core::work::{
    BlockedReason, CancelReason, EdgeKind, RejectionReason, Status, WarningCheck, WorkKind,
};

use super::super::{aging_candidates, cancel_subtree, readiness, Check, Link, SplitMode};
use super::*;

/// P exists with its planning child running, and room in its budget.
fn planned_root() -> Snapshot {
    G::new()
        .add("P")
        .set("P", |p| p.budget = big())
        .child("plan", "P")
        .status("plan", Status::Running)
        .status("P", Status::Running)
        .snap()
}

// ── 18: a cycle through a parent link ──────────────────────────────────

#[test]
fn scenario_18_cycle_through_a_parent_link_is_rejected_whole_then_fixed() {
    let s = planned_root();
    let before = s.items().len();
    // G groups the bookings A and B. G waits on X, and X waits on A: A
    // inherits G's edge on X, so A -> X -> A.
    let mut a = draft("A");
    a.parent = Some(tmp("G"));
    let mut b = draft("B");
    b.parent = Some(tmp("G"));
    let first = plan(
        id("P"),
        vec![draft("G"), a.clone(), b.clone(), draft("X")],
        vec![
            pe(tmp("G"), EdgeKind::Blocks, tmp("X")),
            pe(tmp("X"), EdgeKind::Blocks, tmp("A")),
        ],
    );
    let r = reject(&s, &first, &ctx(FilingSource::Planner));
    assert_eq!(names(&r), vec!["cycle"]);
    let cycle = &r.failed[0];
    assert!(cycle.offending.contains(&tmp("A")), "{cycle:?}");
    assert!(cycle.offending.contains(&tmp("X")), "{cycle:?}");
    assert!(cycle.detail.starts_with("cycle: "), "{}", cycle.detail);
    // Nothing from it exists, and nothing new can be leased.
    assert_eq!(s.items().len(), before);
    assert!(readiness(&s, now()).is_empty());

    // The corrected call: X follows the whole group instead.
    let second = plan(
        id("P"),
        vec![draft("G"), a, b, draft("X")],
        vec![pe(tmp("X"), EdgeKind::Blocks, tmp("G"))],
    );
    let mut s2 = s.clone();
    let accepted = accept(&mut s2, &second, &ctx(FilingSource::Planner));
    let [g, a, b, x] = ids(&accepted, &["G", "A", "B", "X"]).try_into().unwrap();
    assert_eq!(
        accepted.to_core().ids.len(),
        4,
        "real ids for every temp id"
    );
    assert!(
        readiness(&s, now()).is_empty(),
        "only acceptance files items"
    );
    assert_eq!(st(&s2, &a), Status::Ready);
    assert_eq!(st(&s2, &b), Status::Ready);
    assert_eq!(
        st(&s2, &g),
        Status::Running,
        "a parent rolls up, never leased"
    );
    assert_eq!(st(&s2, &x), Status::Queued);
}

// ── 19: validation bounds over-decomposition ───────────────────────────

#[test]
fn scenario_19_twelve_items_for_a_three_step_errand_is_too_many() {
    let s = planned_root();
    let items: Vec<_> = (0..12).map(|i| draft(&format!("s{i}"))).collect();
    let r = reject(
        &s,
        &plan(id("P"), items, vec![]),
        &ctx(FilingSource::Planner),
    );
    assert_eq!(names(&r), vec!["too_many_items"]);
    assert_eq!(r.failed[0].offending, vec![id("P")]);
}

/// The cap is the tree's size, root included: a root with eleven children
/// is twelve items and fits; twelve children under an existing root, with
/// no planning item beside them (a scripted or REST caller), do not.
#[test]
fn scenario_19_the_item_cap_counts_the_root() {
    let bare = G::new().add("P").set("P", |p| p.budget = big()).snap();
    let twelve: Vec<_> = (0..12).map(|i| draft(&format!("s{i}"))).collect();
    let r = reject(
        &bare,
        &plan(id("P"), twelve, vec![]),
        &ctx(FilingSource::Planner),
    );
    assert_eq!(names(&r), vec!["too_many_items"]);

    let mut s = G::new().snap();
    let mut root = draft("R");
    root.parent = None;
    root.budget = Some(big());
    let mut items = vec![root];
    items.extend((0..11).map(|i| {
        let mut d = draft(&format!("s{i}"));
        d.parent = Some(tmp("R"));
        d
    }));
    let accepted = accept(
        &mut s,
        &plan(tmp("R"), items, vec![]),
        &ctx(FilingSource::Planner),
    );
    assert_eq!(accepted.items.len(), 12);
}

#[test]
fn scenario_19_depth_beyond_the_cap_is_rejected() {
    let s = planned_root();
    let mut g = draft("G");
    g.parent = None;
    let mut h = draft("H");
    h.parent = Some(tmp("G"));
    let mut i = draft("I");
    i.parent = Some(tmp("H"));
    // P (1) > G (2) > H (3) > I (4), and the cap is 3 levels.
    let r = reject(
        &s,
        &plan(id("P"), vec![g, h, i], vec![]),
        &ctx(FilingSource::Planner),
    );
    assert_eq!(names(&r), vec!["depth_exceeded"]);
    assert_eq!(r.failed[0].offending, vec![tmp("I")]);
}

#[test]
fn scenario_19_blocks_chain_that_adds_nothing_warns_sequential_split() {
    let mut s = planned_root();
    let p = plan(
        id("P"),
        vec![draft("a"), draft("b")],
        vec![pe(tmp("b"), EdgeKind::Blocks, tmp("a"))],
    );
    let accepted = accept(&mut s, &p, &ctx(FilingSource::Planner));
    assert!(
        accepted.warnings.is_empty(),
        "small sequential slices are allowed by default"
    );
    let mut warning_policy = ctx(FilingSource::Planner);
    warning_policy.sequential_split = SplitMode::Warn;
    let mut s = planned_root();
    let accepted = accept(&mut s, &p, &warning_policy);
    let [a, b] = ids(&accepted, &["a", "b"]).try_into().unwrap();
    assert_eq!(accepted.warnings.len(), 1);
    assert_eq!(accepted.warnings[0].check, WarningCheck::SequentialSplit);
    assert_eq!(accepted.warnings[0].items, vec![a, b]);

    // Once measured, the same check rejects.
    let mut strict = ctx(FilingSource::Planner);
    strict.sequential_split = SplitMode::Reject;
    let r = reject(&planned_root(), &p, &strict);
    assert_eq!(r.checks(), vec![Check::SequentialSplit]);
    let core = r.to_core();
    assert_eq!(core.failed[0].reason, RejectionReason::SequentialSplit);
    assert!(!core.failed[0].detail.is_empty());
}

#[test]
fn scenario_19_a_split_that_adds_something_is_not_flagged() {
    let variants: [fn(&mut WorkItemDraft); 5] = [
        |d| d.trigger = Trigger::At(t(5)),
        |d| d.writable_resources = vec!["calendar".into()],
        |d| d.kind = Some(WorkKind::Research),
        |d| d.worker_kind = rustykrab_core::work::WorkerKind::Local,
        |d| {
            d.preconditions = vec![rustykrab_core::work::Precondition {
                name: "online".into(),
                args: serde_json::Value::Null,
            }]
        },
    ];
    for change in variants {
        let mut b = draft("b");
        change(&mut b);
        let p = plan(
            id("P"),
            vec![draft("a"), b],
            vec![pe(tmp("b"), EdgeKind::Blocks, tmp("a"))],
        );
        let mut s = planned_root();
        let accepted = accept(&mut s, &p, &ctx(FilingSource::Planner));
        assert!(accepted.warnings.is_empty(), "{:?}", accepted.warnings);
    }
}

#[test]
fn scenario_19_children_over_the_root_budget_are_rejected() {
    let s = G::new().snap();
    let mut root = draft("R");
    root.budget = Some(small());
    let mut a = draft("a");
    a.budget = Some(small());
    a.parent = Some(tmp("R"));
    let mut b = draft("b");
    b.budget = Some(small());
    b.parent = Some(tmp("R"));
    let r = reject(
        &s,
        &plan(tmp("R"), vec![root, a, b], vec![]),
        &ctx(FilingSource::Planner),
    );
    assert_eq!(names(&r), vec!["over_budget"]);
    assert_eq!(r.failed[0].offending, vec![tmp("a"), tmp("b")]);
}

#[test]
fn scenario_19_code_item_in_a_planner_graph_is_not_allowed() {
    let s = planned_root();
    let mut c = draft("c");
    c.kind = Some(WorkKind::Code);
    let p = plan(id("P"), vec![c], vec![]);
    let r = reject(&s, &p, &ctx(FilingSource::Planner));
    assert_eq!(names(&r), vec!["kind_not_allowed"]);
    assert_eq!(r.failed[0].offending, vec![tmp("c")]);
    // The delivery import is the one path a code item enters by.
    let mut s = planned_root();
    accept(&mut s, &p, &ctx(FilingSource::DeliveryImport));
}

// ── 20: a failure inside a chain ───────────────────────────────────────

fn chain() -> Snapshot {
    // a blocks b blocks c under P; d is blocked by c and e waits for c.
    G::new()
        .add("P")
        .status("P", Status::Running)
        .child("a", "P")
        .status("a", Status::Done)
        .child("b", "P")
        .status("b", Status::Running)
        .child("c", "P")
        .child("d", "P")
        .child("e", "P")
        .edge("b", EdgeKind::Blocks, "a")
        .edge("c", EdgeKind::Blocks, "b")
        .edge("d", EdgeKind::Blocks, "c")
        .edge("e", EdgeKind::WaitsFor, "c")
        .snap()
}

#[test]
fn scenario_20_nothing_downstream_moves_while_the_ladder_climbs() {
    let mut s = chain();
    // b parks on a capability item at order 2a: its own ladder, not a
    // failure. Nothing cascades.
    let fx = go(&mut s, "b", Status::Blocked(BlockedReason::NeedsTool));
    assert!(fx
        .transitions
        .iter()
        .all(|t| t.item == "b" || t.item == "P"));
    for id in ["c", "d", "e"] {
        assert_eq!(st(&s, id), Status::Queued);
    }
    assert_eq!(st(&s, "P"), Status::Blocked(BlockedReason::NeedsTool));
    assert_eq!(origin(&s, "P").as_deref(), Some("b"));
}

#[test]
fn scenario_20_middle_item_fails_holds_the_chain_with_one_origin() {
    let mut s = chain();
    let fx = go(&mut s, "b", Status::Failed);
    for id in ["c", "d", "e"] {
        assert_eq!(
            st(&s, id),
            Status::Blocked(BlockedReason::UpstreamFailed),
            "{id}"
        );
        assert_eq!(origin(&s, id).as_deref(), Some("b"), "{id}");
    }
    // Each hold is its own event naming its direct upstream.
    let upstream = |id: &str| fx.transition(id).unwrap().upstream.clone();
    assert_eq!(upstream("c").as_deref(), Some("b"));
    assert_eq!(upstream("d").as_deref(), Some("c"));
    assert_eq!(upstream("e").as_deref(), Some("c"));
    assert_eq!(st(&s, "a"), Status::Done);
    assert_eq!(st(&s, "P"), Status::Blocked(BlockedReason::UpstreamFailed));
    assert_eq!(origin(&s, "P").as_deref(), Some("b"));
}

// ── 21: a plan B both ways ─────────────────────────────────────────────

fn plan_b() -> Snapshot {
    G::new()
        .add("P")
        .status("P", Status::Running)
        .child("a", "P")
        .status("a", Status::Running)
        .child("b", "P")
        .edge("b", EdgeKind::ConditionalOnFailure, "a")
        .snap()
}

#[test]
fn scenario_21_plan_b_runs_when_its_step_fails() {
    let mut s = plan_b();
    assert!(
        readiness(&s, now()).is_empty(),
        "never leased while a is live"
    );
    go(&mut s, "a", Status::Failed);
    assert_eq!(st(&s, "b"), Status::Ready);
    go(&mut s, "b", Status::Running);
    go(&mut s, "b", Status::Done);
    // The failed step does not fail the parent: it goes to verification.
    assert_eq!(st(&s, "P"), Status::Verifying);
}

#[test]
fn scenario_21_plan_b_is_cancelled_unleased_when_its_step_succeeds() {
    let mut s = plan_b();
    let fx = go(&mut s, "a", Status::Done);
    assert_eq!(st(&s, "b"), Status::Cancelled(CancelReason::Cascade));
    assert_eq!(origin(&s, "b").as_deref(), Some("a"));
    assert!(fx
        .transitions
        .iter()
        .all(|t| t.item != "b" || t.to == Status::Cancelled(CancelReason::Cascade)));
    assert_eq!(st(&s, "P"), Status::Verifying);
}

// ── 22: fan-in ─────────────────────────────────────────────────────────

#[test]
fn scenario_22_fan_in_defaults_to_the_blocks_upstreams() {
    let mut s = planned_root();
    let mut a = draft("a");
    a.kind = Some(WorkKind::Research);
    let mut b = draft("b");
    b.kind = Some(WorkKind::Research);
    let p = plan(
        id("P"),
        vec![a, b, draft("c"), draft("sibling")],
        vec![
            pe(tmp("c"), EdgeKind::Blocks, tmp("a")),
            pe(tmp("c"), EdgeKind::Blocks, tmp("b")),
        ],
    );
    let accepted = accept(&mut s, &p, &ctx(FilingSource::Planner));
    let [a, b, c] = ids(&accepted, &["a", "b", "c"]).try_into().unwrap();
    assert_eq!(
        s.item(&c).unwrap().inputs_from,
        vec![a.clone(), b.clone()],
        "both upstreams, and nothing from a sibling it did not name"
    );
    go(&mut s, &a, Status::Done);
    assert_eq!(st(&s, &c), Status::Queued, "not ready until both are done");
    go(&mut s, &b, Status::Done);
    assert_eq!(st(&s, &c), Status::Ready);
}

#[test]
fn scenario_22_an_input_the_item_is_not_ordered_after_is_rejected() {
    let s = planned_root();
    let mut c = draft("c");
    c.inputs_from = vec![tmp("a"), tmp("sibling")];
    let p = plan(
        id("P"),
        vec![draft("a"), draft("sibling"), c],
        vec![pe(tmp("c"), EdgeKind::Blocks, tmp("a"))],
    );
    let r = reject(&s, &p, &ctx(FilingSource::Planner));
    assert_eq!(names(&r), vec!["input_unordered"]);
    assert_eq!(r.failed[0].offending, vec![tmp("c"), tmp("sibling")]);
}

#[test]
fn scenario_22_an_input_ordered_through_an_ancestor_is_accepted() {
    // G blocks on a, so G's child k is ordered after a, and after a's
    // own upstream z, transitively.
    let mut s = planned_root();
    let mut k = draft("k");
    k.parent = Some(tmp("G"));
    k.inputs_from = vec![tmp("a"), tmp("z")];
    let p = plan(
        id("P"),
        vec![draft("z"), draft("a"), draft("G"), k],
        vec![
            pe(tmp("a"), EdgeKind::WaitsFor, tmp("z")),
            pe(tmp("G"), EdgeKind::Blocks, tmp("a")),
        ],
    );
    accept(&mut s, &p, &ctx(FilingSource::Planner));
}

// ── 23: cancelling a parent ────────────────────────────────────────────

#[test]
fn scenario_23_parent_cancel_leaves_a_running_child_to_the_controller() {
    let s = G::new()
        .add("P")
        .status("P", Status::Running)
        .child("done", "P")
        .status("done", Status::Done)
        .child("run", "P")
        .status("run", Status::Running)
        .child("q1", "P")
        .child("q2", "P")
        .child("g", "q2")
        .child("check", "P")
        .status("check", Status::Verifying)
        .snap();
    let fx = cancel_subtree(&s, "P", CancelReason::Requested, "P", now());
    assert_eq!(
        fx.status_of("P"),
        Some(Status::Cancelled(CancelReason::Requested))
    );
    for id in ["run", "q1", "q2", "g"] {
        assert_eq!(
            fx.status_of(id),
            Some(Status::Cancelled(CancelReason::Cascade)),
            "{id}"
        );
        assert_eq!(fx.transition(id).unwrap().origin.as_deref(), Some("P"));
    }
    assert_eq!(fx.transition("g").unwrap().upstream.as_deref(), Some("q2"));
    assert_eq!(
        fx.revoke,
        vec!["run".to_string()],
        "lease revoked, evidence kept"
    );
    assert_eq!(fx.verifying, vec!["check".to_string()], "left to finish");
    assert_eq!(
        fx.status_of("done"),
        None,
        "the done child keeps its status"
    );
    assert_eq!(fx.status_of("check"), None);
    // What the reply lists: cancelled, and what had already finished.
    let cancelled: Vec<&str> = fx
        .transitions
        .iter()
        .filter(|t| t.item != "P")
        .map(|t| t.item.as_str())
        .collect();
    assert_eq!(cancelled, vec!["run", "q1", "q2", "g"]);
}

// ── 24: expiry ─────────────────────────────────────────────────────────

#[test]
fn scenario_24_expiry_holds_blocks_releases_waits_for_and_cancels_plan_b() {
    let mut s = G::new()
        .add("P")
        .status("P", Status::Running)
        .child("a", "P")
        .set("a", |a| a.expires_at = Some(t(1)))
        .status("a", Status::Ready)
        .child("b", "P")
        .child("c", "P")
        .child("x", "P")
        .child("q", "P")
        .edge("b", EdgeKind::Blocks, "a")
        .edge("c", EdgeKind::WaitsFor, "a")
        .edge("x", EdgeKind::ConditionalOnFailure, "a")
        .snap();
    assert_eq!(super::super::due_expiries(&s, t(2)), vec!["a".to_string()]);
    let fx = step(&s, "a", Status::Expired, t(2));
    s.apply(&fx, t(2));
    assert_eq!(st(&s, "b"), Status::Blocked(BlockedReason::UpstreamExpired));
    assert_eq!(origin(&s, "b").as_deref(), Some("a"));
    assert_eq!(
        st(&s, "c"),
        Status::Ready,
        "expiry is terminal for waits_for"
    );
    assert_eq!(
        st(&s, "x"),
        Status::Cancelled(CancelReason::Cascade),
        "expiry is not failure: no plan B"
    );

    // Then P itself expires: everything open under it ends, naming P.
    let fx = step(&s, "P", Status::Expired, t(3));
    s.apply(&fx, t(3));
    for id in ["b", "c", "q"] {
        assert_eq!(st(&s, id), Status::Cancelled(CancelReason::Cascade), "{id}");
        assert_eq!(origin(&s, id).as_deref(), Some("P"));
    }
    assert!(readiness(&s, t(3)).is_empty(), "nothing under P runs");
}

// ── 25: re-planning with supersedes ────────────────────────────────────

fn replan_chain() -> Snapshot {
    // a, b, c, d under P; a done, b running on a worker.
    G::new()
        .add("P")
        .set("P", |p| p.budget = big())
        .status("P", Status::Running)
        .child("a", "P")
        .status("a", Status::Done)
        .child("b", "P")
        .status("b", Status::Running)
        .child("c", "P")
        .child("d", "P")
        .child("other", "P")
        .status("other", Status::Running)
        .add("Q")
        .edge("b", EdgeKind::Blocks, "a")
        .edge("c", EdgeKind::Blocks, "b")
        .edge("d", EdgeKind::Blocks, "c")
        .snap()
}

fn worker_ctx() -> FilingContext {
    let mut c = ctx(FilingSource::Discovered);
    c.scope = Some("P".into());
    c.discovered_from = Some("b".into());
    c
}

fn replacements() -> WorkPlan {
    let mut c2 = draft("c2");
    c2.supersedes = Some("c".into());
    c2.edges = vec![rustykrab_core::work::DraftEdge {
        kind: EdgeKind::Blocks,
        depends_on: id("b"),
    }];
    let mut d2 = draft("d2");
    d2.supersedes = Some("d".into());
    d2.edges = vec![rustykrab_core::work::DraftEdge {
        kind: EdgeKind::Blocks,
        depends_on: tmp("c2"),
    }];
    plan(id("P"), vec![c2, d2], vec![])
}

#[test]
fn scenario_25_worker_drafts_supersede_the_rest_of_the_chain() {
    let mut s = replan_chain();
    let accepted = accept(&mut s, &replacements(), &worker_ctx());
    let [c2, d2] = ids(&accepted, &["c2", "d2"]).try_into().unwrap();
    assert!(accepted.supersedes);
    assert_eq!(st(&s, "c"), Status::Cancelled(CancelReason::Superseded));
    assert_eq!(origin(&s, "c").as_deref(), Some(c2.as_str()));
    assert_eq!(st(&s, "d"), Status::Cancelled(CancelReason::Superseded));
    assert_eq!(origin(&s, "d").as_deref(), Some(d2.as_str()));
    // The targets' own ordering edges are dropped; history edges stay.
    let dropped: Vec<(&str, &str)> = accepted
        .effects
        .dropped_edges
        .iter()
        .map(|e| (e.item.as_str(), e.depends_on.as_str()))
        .collect();
    assert_eq!(dropped, vec![("c", "b"), ("d", "c")]);
    assert!(s
        .edges_held_by(&c2)
        .any(|e| e.kind == EdgeKind::DiscoveredFrom && e.depends_on == "b"));
    assert!(s
        .edges_held_by(&c2)
        .any(|e| e.kind == EdgeKind::Supersedes && e.depends_on == "c"));

    assert_eq!(st(&s, &c2), Status::Queued);
    go(&mut s, "b", Status::Done);
    assert_eq!(st(&s, &c2), Status::Ready, "c2 runs when b is done");
    assert_eq!(st(&s, &d2), Status::Queued);
}

#[test]
fn scenario_25_superseding_a_running_item_is_refused_whole() {
    let s = replan_chain();
    let mut p = replacements();
    let mut x = draft("x");
    x.supersedes = Some("other".into());
    p.items.push(x);
    let r = reject(&s, &p, &worker_ctx());
    assert_eq!(names(&r), vec!["supersedes_active"]);
    assert_eq!(r.failed[0].offending, vec![id("other")]);
    // Nothing changed: validation is pure and the snapshot is untouched.
    assert_eq!(st(&s, "c"), Status::Queued);
}

#[test]
fn scenario_25_a_further_replan_beyond_the_rate_is_refused() {
    let s = replan_chain();
    let mut c = worker_ctx();
    c.supersedes_in_window = 2;
    c.supersede_limit = 2;
    let r = reject(&s, &replacements(), &c);
    assert_eq!(names(&r), vec!["rate_limited"]);
}

#[test]
fn scenario_25_closed_and_out_of_scope_targets_are_refused() {
    let s = replan_chain();
    let mut a2 = draft("a2");
    a2.supersedes = Some("a".into());
    let mut q2 = draft("q2");
    q2.supersedes = Some("Q".into());
    let r = reject(&s, &plan(id("P"), vec![a2, q2], vec![]), &worker_ctx());
    assert_eq!(names(&r), vec!["supersedes_closed", "out_of_scope"]);
    assert_eq!(
        r.of(RejectionReason::SupersedesClosed)[0].offending,
        vec![id("a")]
    );
    assert_eq!(
        r.of(RejectionReason::OutOfScope)[0].offending,
        vec![id("Q")]
    );
}

#[test]
fn scenario_25_a_retry_with_replacements_clears_the_held_chain() {
    // b failed after its ladder; c and d are held behind it. The re-plan
    // files a retry b2 and supersedes c with c2 blocked by b2; d re-points
    // to c2 and its hold clears (4.4, 4.5).
    let mut s = replan_chain();
    go(&mut s, "b", Status::Failed);
    assert_eq!(st(&s, "d"), Status::Blocked(BlockedReason::UpstreamFailed));

    let mut c2 = draft("c2");
    c2.supersedes = Some("c".into());
    let p = plan(
        id("P"),
        vec![draft("b2"), c2],
        vec![pe(tmp("c2"), EdgeKind::Blocks, tmp("b2"))],
    );
    let mut c = ctx(FilingSource::Planner);
    c.scope = Some("P".into());
    let accepted = accept(&mut s, &p, &c);
    let [b2, c2] = ids(&accepted, &["b2", "c2"]).try_into().unwrap();

    let r = &accepted.effects.repoints;
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].item, "d");
    assert_eq!(r[0].link, Link::Edge(EdgeKind::Blocks));
    assert_eq!(
        (r[0].old_upstream.as_str(), r[0].new_upstream.as_str()),
        ("c", c2.as_str())
    );
    let cleared = accepted.effects.transition("d").unwrap();
    assert_eq!(cleared.to, Status::Queued);
    assert_eq!(st(&s, "d"), Status::Queued);
    assert_eq!(st(&s, &b2), Status::Ready);

    go(&mut s, &b2, Status::Done);
    go(&mut s, &c2, Status::Done);
    assert_eq!(st(&s, "d"), Status::Ready, "the chain runs again");
}

#[test]
fn scenario_25_superseding_a_parent_supersedes_its_subtree() {
    let s = G::new()
        .add("P")
        .set("P", |p| p.budget = big())
        .child("G", "P")
        .child("g1", "G")
        .child("g2", "G")
        .child("after", "P")
        .edge("after", EdgeKind::Blocks, "g2")
        .snap();
    let mut r = draft("R");
    r.supersedes = Some("G".into());
    let mut r1 = draft("r1");
    r1.supersedes = Some("g1".into());
    r1.parent = Some(tmp("R"));
    let mut s2 = s.clone();
    let accepted = accept(
        &mut s2,
        &plan(id("P"), vec![r, r1], vec![]),
        &ctx(FilingSource::Planner),
    );
    let [rid, r1id] = ids(&accepted, &["R", "r1"]).try_into().unwrap();
    assert_eq!(st(&s2, "G"), Status::Cancelled(CancelReason::Superseded));
    assert_eq!(origin(&s2, "G").as_deref(), Some(rid.as_str()));
    assert_eq!(st(&s2, "g1"), Status::Cancelled(CancelReason::Superseded));
    assert_eq!(origin(&s2, "g1").as_deref(), Some(r1id.as_str()));
    assert_eq!(
        st(&s2, "g2"),
        Status::Cancelled(CancelReason::Cascade),
        "not superseded in the filing"
    );
    assert_eq!(origin(&s2, "g2").as_deref(), Some("G"));
    assert_eq!(
        st(&s2, "after"),
        Status::Cancelled(CancelReason::Cascade),
        "cancellation propagates through blocks"
    );

    // An active descendant refuses the whole filing.
    let busy = G::new()
        .add("P")
        .set("P", |p| p.budget = big())
        .child("G", "P")
        .status("G", Status::Running)
        .child("g1", "G")
        .status("g1", Status::Running)
        .snap();
    let mut r = draft("R");
    r.supersedes = Some("G".into());
    let rej = reject(
        &busy,
        &plan(id("P"), vec![r], vec![]),
        &ctx(FilingSource::Planner),
    );
    let active = rej.of(RejectionReason::SupersedesActive);
    assert_eq!(active.len(), 2, "the running parent, and its running child");
    assert_eq!(active[0].offending, vec![id("G")]);
    assert_eq!(active[1].offending, vec![id("G"), id("g1")]);
}

// ── 26: the delivery import ────────────────────────────────────────────

fn slice(extra: Vec<PlanEdge>) -> WorkPlan {
    let code = |tmp_id: &str, parent: &str| {
        let mut d = draft(tmp_id);
        d.kind = Some(WorkKind::Code);
        d.parent = Some(tmp(parent));
        d.writable_resources = vec!["worktree:slice-7".into()];
        d
    };
    let mut root = draft("slice");
    root.kind = Some(WorkKind::Code);
    root.budget = Some(big());
    let mut l1 = code("L1", "slice");
    l1.done_when = "layer 1 acceptance passes".into();
    l1.writable_resources.clear();
    let mut l2 = code("L2", "slice");
    l2.done_when = "layer 2 acceptance passes".into();
    l2.writable_resources.clear();
    let mut edges = vec![
        pe(tmp("L2"), EdgeKind::Blocks, tmp("L1")),
        pe(tmp("l1b"), EdgeKind::Blocks, tmp("l1a")),
    ];
    edges.extend(extra);
    plan(
        tmp("slice"),
        vec![
            root,
            l1,
            l2,
            code("l1a", "L1"),
            code("l1b", "L1"),
            code("l1c", "L1"),
            code("l2a", "L2"),
        ],
        edges,
    )
}

#[test]
fn scenario_26_an_imported_slice_orders_by_layer_and_dependency() {
    let mut s = G::new().snap();
    let mut c = ctx(FilingSource::DeliveryImport);
    c.default_budget = big();
    let accepted = accept(&mut s, &slice(vec![]), &c);
    assert!(
        accepted.warnings.is_empty(),
        "the import is never re-shaped"
    );
    let [l1, l2, l1a, l1b, l1c, l2a] = ids(&accepted, &["L1", "L2", "l1a", "l1b", "l1c", "l2a"])
        .try_into()
        .unwrap();
    assert_eq!(st(&s, &l1a), Status::Ready);
    assert_eq!(
        st(&s, &l1c),
        Status::Ready,
        "unordered writers in one worktree are allowed"
    );
    assert_eq!(st(&s, &l1b), Status::Queued);
    assert_eq!(st(&s, &l2a), Status::Queued, "gated by L2's edge on L1");

    go(&mut s, &l1a, Status::Done);
    assert_eq!(st(&s, &l1b), Status::Ready);
    go(&mut s, &l1b, Status::Done);
    go(&mut s, &l1c, Status::Done);
    assert_eq!(
        st(&s, &l1),
        Status::Verifying,
        "the delivery verifier decides done"
    );
    go(&mut s, &l1, Status::Done);
    assert_eq!(st(&s, &l2a), Status::Ready);
    assert_eq!(st(&s, &l2), Status::Running);

    // The same writers from a planner would conflict (and be code).
    let r = reject(
        &G::new().snap(),
        &slice(vec![]),
        &ctx(FilingSource::Planner),
    );
    assert!(r.has(RejectionReason::SingleWriterConflict));
    assert!(r.has(RejectionReason::KindNotAllowed));
}

#[test]
fn scenario_26_a_manifest_with_a_cycle_is_rejected_whole() {
    let mut c = ctx(FilingSource::DeliveryImport);
    c.default_budget = big();
    let r = reject(
        &G::new().snap(),
        &slice(vec![pe(tmp("L1"), EdgeKind::Blocks, tmp("L2"))]),
        &c,
    );
    assert_eq!(names(&r), vec!["cycle"]);
    assert!(r.failed[0].offending.contains(&tmp("L1")));
    assert!(r.failed[0].offending.contains(&tmp("L2")));
}

// ── 28: aging ──────────────────────────────────────────────────────────

#[test]
fn scenario_28_old_closed_items_age_unless_something_open_names_them() {
    let mut g = G::new();
    // 500 closed items: 300 closed 60 days ago, 200 closed 5 days ago.
    for i in 0..500 {
        let id = format!("c{i}");
        let closed = if i < 300 { t(-24 * 60) } else { t(-24 * 5) };
        g = g
            .add(&id)
            .status(&id, Status::Done)
            .set(&id, |r| r.closed_at = Some(closed));
    }
    let old = |g: G, id: &str| {
        g.status(id, Status::Done)
            .set(id, |r| r.closed_at = Some(t(-24 * 60)))
    };
    // Held back: named by an open item's inputs_from and blocks edge.
    let mut g = old(g.add("input"), "input");
    g = old(g.add("upstream"), "upstream");
    g = g
        .add("open")
        .set("open", |r| r.inputs_from = vec!["input".into()])
        .edge("open", EdgeKind::Blocks, "upstream");
    // Not held back by provenance.
    g = old(g.add("found"), "found");
    g = g
        .add("follow")
        .edge("follow", EdgeKind::DiscoveredFrom, "found");
    // A subtree ages together: Q is old but its child is recent; R and its
    // child are both old.
    g = old(g.add("Q"), "Q");
    g = g
        .child("q1", "Q")
        .status("q1", Status::Done)
        .set("q1", |r| r.closed_at = Some(t(-24)));
    g = old(g.add("R"), "R");
    g = old(g.child("r1", "R"), "r1");
    // An open parent holds its closed children.
    g = g.add("live");
    g = old(g.child("l1", "live"), "l1");
    let s = g.snap();

    let windows = HashMap::from([(WorkKind::Personal, chrono::TimeDelta::days(30))]);
    let aged = aging_candidates(&s, now(), &windows);
    let has = |id: &str| aged.iter().any(|x| x == id);
    assert_eq!(aged.iter().filter(|x| x.starts_with('c')).count(), 300);
    for open in ["open", "follow", "live"] {
        assert!(!has(open), "{open} is open");
    }
    assert!(!has("input"), "named by inputs_from");
    assert!(!has("upstream"), "named by an ordering edge");
    assert!(has("found"), "discovered_from resolves to the archive line");
    assert!(
        !has("Q") && !has("q1"),
        "the subtree waits for its youngest"
    );
    assert!(has("R") && has("r1"));
    assert!(!has("l1"), "an open parent keeps its children live");
    // A kind without a window never ages.
    assert!(aging_candidates(&s, now(), &HashMap::new()).is_empty());
}

// ── 32: restart in the middle of a graph ───────────────────────────────

fn mid_flight() -> G {
    // One child leased, one held behind a failed sibling, P rolled up.
    G::new()
        .add("P")
        .status("P", Status::Running)
        .child("a", "P")
        .status("a", Status::Failed)
        .child("b", "P")
        .status("b", Status::Blocked(BlockedReason::UpstreamFailed))
        .set("b", |b| b.status_origin = Some("a".into()))
        .child("c", "P")
        .status("c", Status::Leased)
        .edge("b", EdgeKind::Blocks, "a")
}

#[test]
fn scenario_32_restart_re_derives_the_same_state() {
    let s = mid_flight().snap();
    let every: Vec<String> = s.items().iter().map(|i| i.id.clone()).collect();
    assert!(
        settle(&s, &every, now()).is_empty(),
        "readiness and roll-up agree"
    );
    assert!(super::super::hold_recompute_all(&s, &every, now()).is_empty());
    assert!(super::super::stale_ready(&s, now()).is_empty());

    // A half-applied cascade is caught, for a `resume` event.
    let torn = mid_flight().status("b", Status::Queued).snap();
    let fixed = super::super::hold_recompute_all(&torn, &every, now());
    assert_eq!(fixed.len(), 1);
    assert_eq!(fixed[0].item, "b");
    assert_eq!(fixed[0].to, Status::Blocked(BlockedReason::UpstreamFailed));
    assert_eq!(fixed[0].origin.as_deref(), Some("a"));
}
