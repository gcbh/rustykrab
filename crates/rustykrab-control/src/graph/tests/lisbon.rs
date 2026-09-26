//! The Lisbon example of plan section 4.1, end to end.
//!
//! ```text
//! P  Lisbon trip, 3-6 May         parent; expires_at 1 May
//! ├─ A  book a flight             personal
//! ├─ B  book a hotel online       personal
//! ├─ F  book the hotel by email   plan B: conditional_on_failure on B
//! ├─ E  add the trip to calendar  blocked by A, B; inputs_from A, B (the default)
//! └─ T  tell Ana how it went      waits for A, B; inputs_from A, B
//! ```

use rustykrab_core::work::{BlockedReason, CancelReason, EdgeKind, Status, WorkItemId};

use super::super::{cancel_subtree, due_expiries, Link};
use super::*;

struct Trip {
    s: Snapshot,
    a: WorkItemId,
    b: WorkItemId,
    f: WorkItemId,
    e: WorkItemId,
    t: WorkItemId,
}

fn may_first() -> chrono::DateTime<chrono::Utc> {
    t(24 * 11)
}

/// The planner's `work_plan` call under P, accepted and settled. P already
/// has its planning child, reconciled.
fn trip() -> Trip {
    let mut s = G::new()
        .add("P")
        .set("P", |p| {
            p.title = "Lisbon trip, 3-6 May".into();
            p.budget = big();
            p.expires_at = Some(may_first());
        })
        .child("plan", "P")
        .status("plan", Status::Done)
        .snap();
    let mut tell = draft("T");
    tell.inputs_from = vec![tmp("A"), tmp("B")];
    let p = plan(
        id("P"),
        vec![draft("A"), draft("B"), draft("F"), draft("E"), tell],
        vec![
            pe(tmp("F"), EdgeKind::ConditionalOnFailure, tmp("B")),
            pe(tmp("E"), EdgeKind::Blocks, tmp("A")),
            pe(tmp("E"), EdgeKind::Blocks, tmp("B")),
            pe(tmp("T"), EdgeKind::WaitsFor, tmp("A")),
            pe(tmp("T"), EdgeKind::WaitsFor, tmp("B")),
        ],
    );
    let accepted = accept(&mut s, &p, &ctx(FilingSource::Planner));
    assert!(accepted.warnings.is_empty(), "{:?}", accepted.warnings);
    assert!(accepted.held.is_empty());
    let [a, b, f, e, t] = ids(&accepted, &["A", "B", "F", "E", "T"])
        .try_into()
        .unwrap();
    Trip { s, a, b, f, e, t }
}

#[test]
fn lisbon_files_as_one_graph_and_starts_the_independent_steps() {
    let Trip { s, a, b, f, e, t } = trip();
    assert_eq!(st(&s, &a), Status::Ready);
    assert_eq!(st(&s, &b), Status::Ready);
    assert_eq!(st(&s, &f), Status::Queued, "a plan B waits for its step");
    assert_eq!(st(&s, &e), Status::Queued);
    assert_eq!(st(&s, &t), Status::Queued);
    assert_eq!(st(&s, "P"), Status::Running);
    // Fan-in defaults to the blocks upstreams; T's waits_for inputs are
    // listed explicitly.
    assert_eq!(s.item(&e).unwrap().inputs_from, vec![a.clone(), b.clone()]);
    assert_eq!(s.item(&t).unwrap().inputs_from, vec![a, b]);
    // Every new child sits under P, budgeted within P's envelope.
    for id in [&f, &e, &t] {
        let item = s.item(id).unwrap();
        assert_eq!(item.parent.as_deref(), Some("P"));
        assert_eq!(item.expires_at, Some(may_first()), "capped at P's expiry");
        assert!(item.plan_id.is_some());
    }
}

#[test]
fn lisbon_hotel_done_cancels_plan_b_without_a_lease() {
    let Trip {
        mut s,
        a,
        b,
        f,
        e,
        t,
        ..
    } = trip();
    go(&mut s, &b, Status::Running);
    let fx = go(&mut s, &b, Status::Done);

    assert_eq!(st(&s, &f), Status::Cancelled(CancelReason::Cascade));
    assert_eq!(origin(&s, &f).as_deref(), Some(b.as_str()));
    let f_moves: Vec<Status> = fx
        .transitions
        .iter()
        .filter(|x| x.item == f)
        .map(|x| x.to)
        .collect();
    assert_eq!(f_moves, vec![Status::Cancelled(CancelReason::Cascade)]);

    // E and T still wait on the flight.
    assert_eq!(st(&s, &e), Status::Queued);
    assert_eq!(st(&s, &t), Status::Queued);
    go(&mut s, &a, Status::Running);
    go(&mut s, &a, Status::Done);
    assert_eq!(st(&s, &e), Status::Ready);
    assert_eq!(st(&s, &t), Status::Ready);
}

#[test]
fn lisbon_hotel_fails_plan_b_runs_and_takes_over_its_edges() {
    let Trip {
        mut s,
        a,
        b,
        f,
        e,
        t,
        ..
    } = trip();
    go(&mut s, &b, Status::Running);
    let fx = go(&mut s, &b, Status::Failed);

    assert_eq!(st(&s, &f), Status::Ready, "the plan B is released");
    // E's and T's edges on B, and their inputs, move to F instead of
    // taking the failed column.
    let moved: Vec<(String, Link)> = fx
        .repoints
        .iter()
        .map(|r| {
            assert_eq!(r.old_upstream, b);
            assert_eq!(r.new_upstream, f);
            assert_eq!(r.origin, b);
            (r.item.clone(), r.link)
        })
        .collect();
    assert_eq!(
        moved,
        vec![
            (e.clone(), Link::Edge(EdgeKind::Blocks)),
            (t.clone(), Link::Edge(EdgeKind::WaitsFor)),
            (e.clone(), Link::Input),
            (t.clone(), Link::Input),
        ]
    );
    assert_eq!(st(&s, &e), Status::Queued, "not held: the plan B stands in");
    assert_eq!(s.item(&e).unwrap().inputs_from, vec![a.clone(), f.clone()]);
    assert_eq!(s.item(&t).unwrap().inputs_from, vec![a.clone(), f.clone()]);

    go(&mut s, &f, Status::Running);
    go(&mut s, &f, Status::Done);
    assert_eq!(st(&s, &e), Status::Queued, "still waits on the flight");
    go(&mut s, &a, Status::Running);
    go(&mut s, &a, Status::Done);
    assert_eq!(st(&s, &e), Status::Ready);
    assert_eq!(st(&s, &t), Status::Ready);
}

#[test]
fn lisbon_flight_fails_holds_the_calendar_and_ana_still_hears() {
    let Trip {
        mut s,
        a,
        b,
        f,
        e,
        t,
        ..
    } = trip();
    go(&mut s, &a, Status::Running);
    go(&mut s, &a, Status::Failed);

    assert_eq!(st(&s, &e), Status::Blocked(BlockedReason::UpstreamFailed));
    assert_eq!(origin(&s, &e).as_deref(), Some(a.as_str()));
    assert_eq!(
        st(&s, &t),
        Status::Queued,
        "T waits for the hotel branch too"
    );
    assert_eq!(st(&s, "P"), Status::Running, "the hotel is still moving");

    go(&mut s, &b, Status::Running);
    go(&mut s, &b, Status::Done);
    assert_eq!(st(&s, &f), Status::Cancelled(CancelReason::Cascade));
    assert_eq!(
        st(&s, &t),
        Status::Ready,
        "waits_for is satisfied by a failure"
    );
    go(&mut s, &t, Status::Running);
    go(&mut s, &t, Status::Done);

    // One cause for the whole parent: P names A.
    assert_eq!(st(&s, "P"), Status::Blocked(BlockedReason::UpstreamFailed));
    assert_eq!(origin(&s, "P").as_deref(), Some(a.as_str()));
}

#[test]
fn lisbon_parent_expiry_cancels_everything_open_under_it() {
    let Trip {
        mut s,
        a,
        b,
        f,
        e,
        t,
        ..
    } = trip();
    go(&mut s, &a, Status::Running);

    assert!(due_expiries(&s, now()).is_empty());
    assert_eq!(due_expiries(&s, may_first()), vec!["P".to_string()]);

    let fx = go(&mut s, "P", Status::Expired);
    for id in [&a, &b, &f, &e, &t] {
        assert_eq!(st(&s, id), Status::Cancelled(CancelReason::Cascade), "{id}");
        assert_eq!(origin(&s, id).as_deref(), Some("P"));
    }
    assert_eq!(
        fx.revoke,
        vec![a.clone()],
        "the running flight's lease goes"
    );
    assert_eq!(
        st(&s, "plan"),
        Status::Done,
        "closed children keep their status"
    );
}

#[test]
fn lisbon_parent_cancel_cancels_everything_open_under_it() {
    let Trip {
        s, a, b, f, e, t, ..
    } = trip();
    let fx = cancel_subtree(&s, "P", CancelReason::Requested, "P", now());
    assert_eq!(
        fx.status_of("P"),
        Some(Status::Cancelled(CancelReason::Requested))
    );
    for id in [&a, &b, &f, &e, &t] {
        assert_eq!(
            fx.status_of(id),
            Some(Status::Cancelled(CancelReason::Cascade))
        );
        assert_eq!(fx.transition(id).unwrap().origin.as_deref(), Some("P"));
    }
    assert!(fx.revoke.is_empty(), "nothing was leased");
}
