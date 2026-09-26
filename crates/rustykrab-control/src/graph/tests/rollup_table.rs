//! Every row of plan section 4.2's roll-up table, first match wins.

use rustykrab_core::work::{BlockedReason, CancelReason, EdgeKind, Status};

use super::super::{rollup, rollup_all, Rollup};
use super::*;

fn parent_with(children: &[(&str, Status)]) -> G {
    let mut g = G::new().add("P");
    for (id, s) in children {
        g = g.child(id, "P").status(id, *s);
    }
    g
}

fn rolled(s: &Snapshot, id: &str) -> Rollup {
    rollup(s, id, now()).expect("a parent rolls up")
}

// Row 1: the parent's own gate is shut.

#[test]
fn row_1_a_trigger_not_yet_fired_rolls_up_queued() {
    let s = parent_with(&[("a", Status::Queued)])
        .set("P", |p| p.trigger = Trigger::At(t(5)))
        .snap();
    assert_eq!(rolled(&s, "P").status, Status::Queued);
    let later = rollup(&s, "P", t(5)).unwrap();
    assert_eq!(later.status, Status::Queued, "fired, but a still waits");
}

#[test]
fn row_1_a_pending_approval_rolls_up_needs_consent() {
    let s = parent_with(&[("a", Status::Ready)])
        .set("P", |p| p.held_by = Some("q-1".into()))
        .snap();
    assert_eq!(
        rolled(&s, "P"),
        Rollup {
            status: Status::Blocked(BlockedReason::NeedsConsent),
            origin: Some("P".into()),
        }
    );
}

#[test]
fn row_1_the_parents_own_edges_gate_it_and_carry_their_hold() {
    let waiting = parent_with(&[("a", Status::Queued)])
        .add("X")
        .edge("P", EdgeKind::Blocks, "X")
        .snap();
    assert_eq!(rolled(&waiting, "P").status, Status::Queued);

    let held = parent_with(&[("a", Status::Queued)])
        .add("X")
        .status("X", Status::Failed)
        .edge("P", EdgeKind::Blocks, "X")
        .snap();
    assert_eq!(
        rolled(&held, "P"),
        Rollup {
            status: Status::Blocked(BlockedReason::UpstreamFailed),
            origin: Some("X".into()),
        }
    );
}

// Row 2: any descendant ready, leased, running or verifying.

#[test]
fn row_2_any_descendant_moving_rolls_up_running() {
    for moving in [
        Status::Ready,
        Status::Leased,
        Status::Running,
        Status::Verifying,
    ] {
        // The moving item is a grandchild, beside a blocked child: row 2
        // wins over row 3.
        let s = parent_with(&[
            ("mid", Status::Queued),
            ("stuck", Status::Blocked(BlockedReason::NeedsDecision)),
        ])
        .child("leaf", "mid")
        .status("leaf", moving)
        .snap();
        assert_eq!(rolled(&s, "P").status, Status::Running, "{moving}");
    }
}

// Row 3: otherwise any descendant blocked.

#[test]
fn row_3_a_reason_that_needs_the_user_comes_first() {
    let s = parent_with(&[
        ("held", Status::Blocked(BlockedReason::UpstreamFailed)),
        ("cred", Status::Blocked(BlockedReason::NeedsCredential)),
    ])
    .set("held", |r| {
        r.status_origin = Some("X".into());
        r.updated_at = t(-10);
    })
    .set("cred", |r| r.updated_at = t(-1))
    .snap();
    assert_eq!(
        rolled(&s, "P"),
        Rollup {
            status: Status::Blocked(BlockedReason::NeedsCredential),
            origin: Some("cred".into()),
        }
    );
}

#[test]
fn row_3_otherwise_the_earliest_block_with_its_origin() {
    let s = parent_with(&[
        ("tool", Status::Blocked(BlockedReason::NeedsTool)),
        ("held", Status::Blocked(BlockedReason::UpstreamFailed)),
        ("done", Status::Done),
    ])
    .set("tool", |r| r.updated_at = t(-1))
    .set("held", |r| {
        r.status_origin = Some("X".into());
        r.updated_at = t(-10);
    })
    .snap();
    assert_eq!(
        rolled(&s, "P"),
        Rollup {
            status: Status::Blocked(BlockedReason::UpstreamFailed),
            origin: Some("X".into()),
        }
    );
}

// Row 4: otherwise open descendants waiting on edges or triggers.

#[test]
fn row_4_open_descendants_waiting_roll_up_queued() {
    let s = parent_with(&[("a", Status::Done), ("b", Status::Queued)])
        .set("b", |b| b.trigger = Trigger::OnAnswer("which hotel".into()))
        .snap();
    assert_eq!(
        rolled(&s, "P"),
        Rollup {
            status: Status::Queued,
            origin: None
        }
    );
}

// Row 5: every child closed.

#[test]
fn row_5_every_child_closed_rolls_up_verifying_even_with_a_failure() {
    // A failed child whose plan B booked the hotel does not fail the
    // parent: `done_when` decides, in verification.
    let s = parent_with(&[
        ("online", Status::Failed),
        ("email", Status::Done),
        ("unused", Status::Cancelled(CancelReason::Cascade)),
    ])
    .snap();
    assert_eq!(rolled(&s, "P").status, Status::Verifying);
}

// Overrides and edges of the table.

#[test]
fn a_parents_own_cancel_or_expiry_overrides_the_table() {
    for closed in [Status::Cancelled(CancelReason::Requested), Status::Expired] {
        let s = parent_with(&[("a", Status::Running)])
            .status("P", closed)
            .snap();
        assert_eq!(rolled(&s, "P").status, closed);
        assert!(rollup_all(&s, &["a".into()], now()).is_empty());
    }
}

#[test]
fn a_leaf_has_no_rollup() {
    let s = G::new().add("a").snap();
    assert_eq!(rollup(&s, "a", now()), None);
}

#[test]
fn rollup_all_goes_bottom_up_through_every_ancestor() {
    let s = G::new()
        .add("top")
        .child("mid", "top")
        .child("leaf", "mid")
        .status("leaf", Status::Running)
        .snap();
    let moves = rollup_all(&s, &["leaf".into()], now());
    let order: Vec<(&str, Status)> = moves.iter().map(|t| (t.item.as_str(), t.to)).collect();
    assert_eq!(
        order,
        vec![("mid", Status::Running), ("top", Status::Running)]
    );
}

#[test]
fn rollup_all_keeps_the_parents_own_parked_states() {
    let parked = parent_with(&[("a", Status::Queued)])
        .status("P", Status::Blocked(BlockedReason::BudgetExhausted))
        .snap();
    assert!(rollup_all(&parked, &["a".into()], now()).is_empty());

    let unverified = parent_with(&[("a", Status::Done)])
        .status("P", Status::Blocked(BlockedReason::VerificationFailed))
        .snap();
    assert!(
        rollup_all(&unverified, &["a".into()], now()).is_empty(),
        "waits for its ladder's re-plan"
    );
    // A re-plan's new child moves it again.
    let replanned = parent_with(&[("a", Status::Done), ("fix", Status::Ready)])
        .status("P", Status::Blocked(BlockedReason::VerificationFailed))
        .snap();
    let moves = rollup_all(&replanned, &["fix".into()], now());
    assert_eq!(moves[0].to, Status::Running);
}
