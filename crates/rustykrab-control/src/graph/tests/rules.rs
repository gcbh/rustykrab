//! Readiness (4.1, 4.2), cascade and holds (4.5), plan B chains (6.4) and
//! approval holds (6.1), rule by rule.

use std::collections::BTreeSet;

use rustykrab_core::work::{BlockedReason, CancelReason, EdgeKind, Status, WorkKind};

use super::super::{
    due_expiries, hold_recompute, hold_recompute_all, is_ready, readiness, stale_ready, supersede,
    ApprovalPolicy, ApprovalTrigger, Link, Repoint,
};
use super::*;

// ── readiness ──────────────────────────────────────────────────────────

#[test]
fn readiness_follows_each_edge_kind() {
    use EdgeKind::*;
    let cases = [
        (Blocks, Status::Done, true),
        (Blocks, Status::Failed, false),
        (Blocks, Status::Running, false),
        (WaitsFor, Status::Done, true),
        (WaitsFor, Status::Failed, true),
        (WaitsFor, Status::Expired, true),
        (WaitsFor, Status::Cancelled(CancelReason::Requested), true),
        (WaitsFor, Status::Running, false),
        (ConditionalOnFailure, Status::Failed, true),
        (ConditionalOnFailure, Status::Done, false),
        (ConditionalOnFailure, Status::Running, false),
        (Supersedes, Status::Running, true),
        (DiscoveredFrom, Status::Running, true),
    ];
    for (kind, upstream, ready) in cases {
        let s = G::new()
            .add("u")
            .status("u", upstream)
            .add("d")
            .edge("d", kind, "u")
            .snap();
        assert_eq!(is_ready(&s, "d", now()), ready, "{kind:?} on {upstream}");
    }
}

#[test]
fn an_upstream_missing_from_the_snapshot_never_satisfies() {
    let s = G::new()
        .add("d")
        .edge("d", EdgeKind::WaitsFor, "archived")
        .snap();
    assert!(!is_ready(&s, "d", now()));
}

#[test]
fn triggers_fire_by_time_or_when_the_snapshot_says_so() {
    let s = G::new()
        .add("later")
        .set("later", |i| i.trigger = Trigger::At(t(2)))
        .add("answer")
        .set("answer", |i| i.trigger = Trigger::OnAnswer("q-7".into()))
        .snap();
    assert!(readiness(&s, now()).is_empty());
    assert_eq!(readiness(&s, t(2)), vec!["later".to_string()]);
    let fired = s.clone().with_fired(["answer".to_string()]);
    assert_eq!(readiness(&fired, now()), vec!["answer".to_string()]);
}

#[test]
fn an_expired_or_approval_held_item_is_not_ready() {
    let s = G::new()
        .add("old")
        .set("old", |i| i.expires_at = Some(t(-1)))
        .add("held")
        .set("held", |i| i.held_by = Some("q-1".into()))
        .snap();
    assert!(readiness(&s, now()).is_empty());
}

#[test]
fn an_item_with_children_is_never_ready() {
    let s = G::new().add("P").child("a", "P").snap();
    assert_eq!(readiness(&s, now()), vec!["a".to_string()]);
}

#[test]
fn every_ancestor_gates_its_subtree() {
    let tree = || G::new().add("top").child("mid", "top").child("leaf", "mid");

    let timed = tree().set("top", |i| i.trigger = Trigger::At(t(5))).snap();
    assert!(!is_ready(&timed, "leaf", now()));
    assert!(is_ready(&timed, "leaf", t(5)));

    let held = tree().set("mid", |i| i.held_by = Some("q".into())).snap();
    assert!(!is_ready(&held, "leaf", now()));

    let ordered = tree().add("X").edge("top", EdgeKind::Blocks, "X");
    assert!(!is_ready(&ordered.snap(), "leaf", now()));
    assert!(is_ready(
        &ordered.status("X", Status::Done).snap(),
        "leaf",
        now()
    ));

    let expired = tree().set("top", |i| i.expires_at = Some(t(-1))).snap();
    assert!(!is_ready(&expired, "leaf", now()));

    let closed = tree()
        .status("mid", Status::Cancelled(CancelReason::Requested))
        .snap();
    assert!(!is_ready(&closed, "leaf", now()));
}

#[test]
fn stale_ready_finds_ready_items_whose_gate_shut() {
    let s = G::new()
        .add("up")
        .add("r")
        .status("r", Status::Ready)
        .edge("r", EdgeKind::Blocks, "up")
        .add("ok")
        .status("ok", Status::Ready)
        .snap();
    assert_eq!(stale_ready(&s, now()), vec!["r".to_string()]);
    let fx = settle(&s, &[], now());
    assert_eq!(fx.status_of("r"), Some(Status::Queued));
    assert_eq!(fx.status_of("ok"), None);
}

#[test]
fn due_expiries_leave_items_under_a_due_ancestor_to_the_cascade() {
    let s = G::new()
        .add("P")
        .set("P", |i| i.expires_at = Some(t(1)))
        .child("a", "P")
        .set("a", |i| i.expires_at = Some(t(1)))
        .add("solo")
        .set("solo", |i| i.expires_at = Some(t(3)))
        .snap();
    assert_eq!(due_expiries(&s, t(2)), vec!["P".to_string()]);
    assert_eq!(
        due_expiries(&s, t(3)),
        vec!["P".to_string(), "solo".to_string()]
    );
}

// ── cascade ────────────────────────────────────────────────────────────

#[test]
fn cancellation_cascades_through_closed_items() {
    let mut s = G::new()
        .add("a")
        .add("b")
        .add("c")
        .add("d")
        .add("e")
        .edge("b", EdgeKind::Blocks, "a")
        .edge("c", EdgeKind::Blocks, "b")
        .edge("d", EdgeKind::WaitsFor, "b")
        .edge("e", EdgeKind::ConditionalOnFailure, "b")
        .snap();
    let fx = go(&mut s, "a", Status::Cancelled(CancelReason::Requested));
    for id in ["b", "c", "e"] {
        assert_eq!(st(&s, id), Status::Cancelled(CancelReason::Cascade), "{id}");
        assert_eq!(origin(&s, id).as_deref(), Some("a"), "{id}");
    }
    assert_eq!(fx.transition("c").unwrap().upstream.as_deref(), Some("b"));
    assert_eq!(
        st(&s, "d"),
        Status::Ready,
        "a cancelled upstream closes waits_for"
    );
}

#[test]
fn holds_spread_through_open_items_but_never_reach_active_or_closed_ones() {
    let mut s = G::new()
        .add("a")
        .status("a", Status::Running)
        .add("b")
        .add("c")
        .add("busy")
        .status("busy", Status::Running)
        .add("over")
        .status("over", Status::Done)
        .edge("b", EdgeKind::Blocks, "a")
        .edge("c", EdgeKind::WaitsFor, "b")
        .edge("busy", EdgeKind::Blocks, "a")
        .edge("over", EdgeKind::Blocks, "a")
        .snap();
    go(&mut s, "a", Status::Failed);
    assert_eq!(st(&s, "b"), Status::Blocked(BlockedReason::UpstreamFailed));
    assert_eq!(st(&s, "c"), Status::Blocked(BlockedReason::UpstreamFailed));
    assert_eq!(origin(&s, "c").as_deref(), Some("a"));
    assert_eq!(st(&s, "busy"), Status::Running);
    assert_eq!(st(&s, "over"), Status::Done);
}

#[test]
fn a_held_item_keeps_its_first_cause() {
    let mut s = G::new()
        .add("a")
        .add("b")
        .add("c")
        .edge("c", EdgeKind::Blocks, "a")
        .edge("c", EdgeKind::Blocks, "b")
        .snap();
    go(&mut s, "a", Status::Failed);
    let fx = go(&mut s, "b", Status::Expired);
    assert_eq!(fx.status_of("c"), None);
    assert_eq!(st(&s, "c"), Status::Blocked(BlockedReason::UpstreamFailed));
    assert_eq!(origin(&s, "c").as_deref(), Some("a"));
}

#[test]
fn step_on_a_closed_item_does_nothing() {
    let s = G::new().add("a").status("a", Status::Done).snap();
    assert!(step(&s, "a", Status::Failed, now()).is_empty());
}

// ── holds clear only by change ─────────────────────────────────────────

fn held_chain() -> Snapshot {
    G::new()
        .add("a")
        .status("a", Status::Failed)
        .add("a2")
        .add("b")
        .status("b", Status::Blocked(BlockedReason::UpstreamFailed))
        .set("b", |i| i.status_origin = Some("a".into()))
        .add("c")
        .status("c", Status::Blocked(BlockedReason::UpstreamFailed))
        .set("c", |i| i.status_origin = Some("a".into()))
        .edge("b", EdgeKind::Blocks, "a")
        .edge("c", EdgeKind::WaitsFor, "b")
        .snap()
}

fn repoint_b_to_a2(s: &mut Snapshot) {
    let fx = Effects {
        repoints: vec![Repoint {
            item: "b".into(),
            link: Link::Edge(EdgeKind::Blocks),
            old_upstream: "a".into(),
            new_upstream: "a2".into(),
            origin: "a2".into(),
        }],
        ..Effects::default()
    };
    s.apply(&fx, now());
}

#[test]
fn hold_recompute_keeps_a_hold_until_the_path_changes() {
    let mut s = held_chain();
    assert_eq!(hold_recompute(&s, "b"), None, "still behind the failure");
    repoint_b_to_a2(&mut s);
    let cleared = hold_recompute(&s, "b").unwrap();
    assert_eq!(cleared.to, Status::Queued);
    // And the chain behind it clears with it.
    let all = hold_recompute_all(&s, &["b".into()], now());
    let moves: Vec<(&str, Status)> = all.iter().map(|t| (t.item.as_str(), t.to)).collect();
    assert_eq!(moves, vec![("b", Status::Queued), ("c", Status::Queued)]);
}

#[test]
fn hold_recompute_leaves_an_items_own_block_alone_and_can_hold() {
    let own = G::new()
        .add("x")
        .status("x", Status::Blocked(BlockedReason::NeedsTool))
        .snap();
    assert_eq!(hold_recompute(&own, "x"), None);

    let onto_failed = G::new()
        .add("f")
        .status("f", Status::Failed)
        .add("q")
        .edge("q", EdgeKind::Blocks, "f")
        .snap();
    let t = hold_recompute(&onto_failed, "q").unwrap();
    assert_eq!(t.to, Status::Blocked(BlockedReason::UpstreamFailed));
    assert_eq!(t.origin.as_deref(), Some("f"));
}

// ── plan B ─────────────────────────────────────────────────────────────

#[test]
fn a_plan_b_may_have_its_own_plan_b() {
    let mut s = G::new()
        .add("B")
        .status("B", Status::Running)
        .add("F")
        .add("G2")
        .add("E")
        .set("E", |e| e.inputs_from = vec!["B".into()])
        .edge("F", EdgeKind::ConditionalOnFailure, "B")
        .edge("G2", EdgeKind::ConditionalOnFailure, "F")
        .edge("E", EdgeKind::Blocks, "B")
        .snap();
    go(&mut s, "B", Status::Failed);
    assert_eq!(st(&s, "F"), Status::Ready);
    assert!(s.edges_held_by("E").any(|e| e.depends_on == "F"));
    go(&mut s, "F", Status::Running);
    go(&mut s, "F", Status::Failed);
    assert_eq!(st(&s, "G2"), Status::Ready);
    assert!(s.edges_held_by("E").any(|e| e.depends_on == "G2"));
    assert_eq!(s.item("E").unwrap().inputs_from, vec!["G2".to_string()]);
    assert_eq!(
        st(&s, "E"),
        Status::Queued,
        "never held while a plan B stands in"
    );
    go(&mut s, "G2", Status::Done);
    assert_eq!(st(&s, "E"), Status::Ready);
}

#[test]
fn a_plan_b_keeps_its_step_as_an_input() {
    let mut s = G::new()
        .add("B")
        .add("F")
        .set("F", |f| f.inputs_from = vec!["B".into()])
        .edge("F", EdgeKind::ConditionalOnFailure, "B")
        .snap();
    let fx = go(&mut s, "B", Status::Failed);
    assert!(fx.repoints.is_empty());
    assert_eq!(s.item("F").unwrap().inputs_from, vec!["B".to_string()]);
}

// ── filings that touch existing items ──────────────────────────────────

#[test]
fn a_ready_item_that_gains_an_unsatisfied_edge_returns_to_queued() {
    let mut s = G::new()
        .add("P")
        .set("P", |p| p.budget = big())
        .child("r", "P")
        .status("r", Status::Ready)
        .snap();
    let p = plan(
        id("P"),
        vec![draft("k")],
        vec![pe(id("r"), EdgeKind::Blocks, tmp("k"))],
    );
    let accepted = accept(&mut s, &p, &ctx(FilingSource::Planner));
    let k = accepted.id("k").unwrap().clone();
    assert_eq!(accepted.effects.status_of("r"), Some(Status::Queued));
    assert_eq!(st(&s, "r"), Status::Queued);
    assert_eq!(st(&s, &k), Status::Ready);
    go(&mut s, &k, Status::Done);
    assert_eq!(st(&s, "r"), Status::Ready);
}

#[test]
fn the_ladder_parks_an_item_on_a_capability_item() {
    let mut s = G::new()
        .add("P")
        .child("x", "P")
        .status("x", Status::Blocked(BlockedReason::NeedsTool))
        .snap();
    let mut cap = draft("cap");
    cap.kind = Some(WorkKind::Capability);
    let p = plan(
        tmp("cap"),
        vec![cap],
        vec![pe(id("x"), EdgeKind::Blocks, tmp("cap"))],
    );
    let accepted = accept(&mut s, &p, &ctx(FilingSource::Ladder));
    let cap = accepted.root.clone();
    assert_eq!(s.item(&cap).unwrap().parent, None, "no parent in the chain");
    assert_eq!(st(&s, &cap), Status::Ready);
    go(&mut s, &cap, Status::Failed);
    assert_eq!(st(&s, "x"), Status::Blocked(BlockedReason::UpstreamFailed));
    assert_eq!(origin(&s, "x").as_deref(), Some(cap.as_str()));
}

#[test]
fn supersede_applies_on_its_own() {
    let s = G::new()
        .add("old")
        .add("new")
        .add("dep")
        .set("dep", |d| d.inputs_from = vec!["old".into()])
        .add("up")
        .edge("old", EdgeKind::Blocks, "up")
        .edge("dep", EdgeKind::Blocks, "old")
        .snap();
    let fx = supersede(&s, &[("new".into(), "old".into())], now());
    assert_eq!(
        fx.status_of("old"),
        Some(Status::Cancelled(CancelReason::Superseded))
    );
    let links: Vec<Link> = fx.repoints.iter().map(|r| r.link).collect();
    assert_eq!(links, vec![Link::Edge(EdgeKind::Blocks), Link::Input]);
    assert_eq!(fx.dropped_edges.len(), 1);
    assert_eq!(fx.dropped_edges[0].depends_on, "up");
}

// ── approval (6.1) ─────────────────────────────────────────────────────

/// "Compare three phone plans and switch me to the cheapest by Friday."
fn phone_plans(policy: ApprovalPolicy) -> (Snapshot, Accepted) {
    let mut s = G::new().add("P").set("P", |p| p.budget = big()).snap();
    let research = |k: &str| {
        let mut d = draft(k);
        d.kind = Some(WorkKind::Research);
        d
    };
    let mut e = draft("e");
    e.writable_resources = vec!["carrier account".into()];
    let p = plan(
        id("P"),
        vec![
            research("a"),
            research("b"),
            research("c"),
            draft("d"),
            e,
            draft("f"),
        ],
        vec![
            pe(tmp("d"), EdgeKind::Blocks, tmp("a")),
            pe(tmp("d"), EdgeKind::Blocks, tmp("b")),
            pe(tmp("d"), EdgeKind::Blocks, tmp("c")),
            pe(tmp("e"), EdgeKind::Blocks, tmp("d")),
            pe(tmp("f"), EdgeKind::ConditionalOnFailure, tmp("e")),
        ],
    );
    let mut c = ctx(FilingSource::Planner);
    c.approval = policy;
    let accepted = accept(&mut s, &p, &c);
    (s, accepted)
}

#[test]
fn approval_holds_an_undelegated_writer_and_what_depends_on_it() {
    let (mut s, accepted) = phone_plans(ApprovalPolicy {
        id: Some("policy-1".into()),
        delegated_resources: Some(BTreeSet::new()),
        ..ApprovalPolicy::default()
    });
    let [a, b, c, d, e, f] = ids(&accepted, &["a", "b", "c", "d", "e", "f"])
        .try_into()
        .unwrap();
    assert_eq!(accepted.held, vec![e.clone(), f.clone()]);
    assert_eq!(
        accepted.triggers,
        vec![ApprovalTrigger::UndelegatedResource {
            item: e.clone(),
            resource: "carrier account".into(),
        }]
    );
    assert_eq!(accepted.policy.as_deref(), Some("policy-1"));
    let question = accepted.question.clone().unwrap();
    for id in [&e, &f] {
        let item = s.item(id).unwrap();
        assert_eq!(item.status, Status::Blocked(BlockedReason::NeedsConsent));
        assert_eq!(item.held_by.as_deref(), Some(question.as_str()));
    }
    for id in [&a, &b, &c] {
        assert_eq!(st(&s, id), Status::Ready, "unheld siblings start at once");
    }
    assert_eq!(st(&s, &d), Status::Queued);
    assert_eq!(accepted.to_core().held, vec![e.clone(), f.clone()]);

    // Declined: the held items are cancelled, cascading per 4.5.
    go(&mut s, &e, Status::Cancelled(CancelReason::Requested));
    assert_eq!(st(&s, &f), Status::Cancelled(CancelReason::Cascade));
}

#[test]
fn approval_over_a_whole_graph_threshold_holds_every_new_item() {
    for policy in [
        ApprovalPolicy {
            max_items: Some(5),
            ..ApprovalPolicy::default()
        },
        ApprovalPolicy {
            max_total_tokens: Some(1_000),
            ..ApprovalPolicy::default()
        },
    ] {
        let (s, accepted) = phone_plans(policy);
        assert_eq!(accepted.held.len(), 6);
        assert!(readiness(&s, now()).is_empty());
        assert_eq!(st(&s, "P"), Status::Blocked(BlockedReason::NeedsConsent));
    }
}

#[test]
fn nothing_is_held_when_standing_judgment_covers_every_trigger() {
    let (_, accepted) = phone_plans(ApprovalPolicy {
        delegated_resources: Some(BTreeSet::from(["carrier account".to_string()])),
        max_items: Some(6),
        ..ApprovalPolicy::default()
    });
    assert!(accepted.held.is_empty());
    assert_eq!(accepted.question, None);
    assert_eq!(accepted.policy, None);
}

#[test]
fn code_outside_an_authorized_slice_is_held_and_is_an_approval_point() {
    let code = |k: &str, parent: &str| {
        let mut d = draft(k);
        d.kind = Some(WorkKind::Code);
        d.parent = Some(id(parent));
        d
    };
    let p = plan(
        id("top"),
        vec![code("inside", "S"), code("outside", "P")],
        vec![pe(tmp("outside"), EdgeKind::Blocks, tmp("inside"))],
    );
    let mut c = ctx(FilingSource::Proposal);
    c.allow_code = true;
    c.approval = ApprovalPolicy {
        authorized_slices: Some(BTreeSet::from(["S".to_string()])),
        ..ApprovalPolicy::default()
    };
    let mut s = two_parents();
    let accepted = accept(&mut s, &p, &c);
    let [inside, outside] = ids(&accepted, &["inside", "outside"]).try_into().unwrap();
    assert_eq!(accepted.held, vec![outside.clone()]);
    assert_eq!(
        accepted.triggers,
        vec![ApprovalTrigger::CodeOutsideSlice {
            item: outside.clone()
        }]
    );
    assert!(
        accepted.warnings.is_empty(),
        "the approval point earns the split"
    );
    assert_eq!(st(&s, &inside), Status::Ready);
    assert_eq!(
        st(&s, &outside),
        Status::Blocked(BlockedReason::NeedsConsent)
    );

    // Explicit legacy warning policy can still report an unneeded split.
    c.approval = ApprovalPolicy::default();
    c.sequential_split = crate::graph::SplitMode::Warn;
    let accepted = accept(&mut two_parents(), &p, &c);
    assert_eq!(accepted.warnings.len(), 1);
}

#[test]
fn hold_discovered_holds_a_workers_follow_ups_whole_and_nothing_else() {
    let p = plan(
        id("P"),
        vec![draft("a"), draft("b")],
        vec![pe(tmp("b"), EdgeKind::Blocks, tmp("a"))],
    );
    let policy = ApprovalPolicy {
        hold_discovered: true,
        ..ApprovalPolicy::default()
    };

    let mut c = ctx(FilingSource::Discovered);
    c.approval = policy.clone();
    let mut s = two_parents();
    let accepted = accept(&mut s, &p, &c);
    let [a, b] = ids(&accepted, &["a", "b"]).try_into().unwrap();
    assert_eq!(accepted.held, vec![a.clone(), b.clone()]);
    assert_eq!(
        accepted.triggers,
        vec![ApprovalTrigger::Discovered { items: 2 }]
    );
    assert_eq!(st(&s, &a), Status::Blocked(BlockedReason::NeedsConsent));

    // The same graph filed through `work_file` is not a worker's follow-up.
    let mut c = ctx(FilingSource::WorkFile);
    c.approval = policy.clone();
    let accepted = accept(&mut two_parents(), &p, &c);
    assert!(accepted.held.is_empty());
    assert!(accepted.triggers.is_empty());

    // Every ladder filing is held the same way: an `internal` item, and a
    // `capability` build, which would otherwise write a skill at once.
    let mut c = ctx(FilingSource::Ladder);
    c.approval = policy;
    for kind in [WorkKind::Internal, WorkKind::Capability] {
        let mut d = draft("i");
        d.kind = Some(kind);
        if kind == WorkKind::Capability {
            d.title = "Build tool: weather_lookup".to_string();
        }
        let mut s = two_parents();
        let accepted = accept(&mut s, &plan(id("P"), vec![d], vec![]), &c);
        let [i] = ids(&accepted, &["i"]).try_into().unwrap();
        assert_eq!(
            accepted.triggers,
            vec![ApprovalTrigger::Ladder { items: 1 }]
        );
        assert_eq!(st(&s, &i), Status::Blocked(BlockedReason::NeedsConsent));
    }

    // Without the switch the ladder's filings are system work, unheld.
    c.approval = ApprovalPolicy::default();
    let mut d = draft("i");
    d.kind = Some(WorkKind::Internal);
    let accepted = accept(&mut two_parents(), &plan(id("P"), vec![d], vec![]), &c);
    assert!(accepted.held.is_empty());
}

/// `top` with two child parents: `S`, an authorised slice, and `P`.
fn two_parents() -> Snapshot {
    G::new()
        .add("top")
        .set("top", |p| p.budget = big())
        .child("S", "top")
        .set("S", |p| p.budget = big())
        .child("P", "top")
        .set("P", |p| p.budget = big())
        .snap()
}
