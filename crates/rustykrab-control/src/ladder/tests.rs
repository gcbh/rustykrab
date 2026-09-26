use chrono::{DateTime, TimeZone, Utc};
use rustykrab_core::work::{
    BlockedReason, ErrorSubclass, Rung, RungBudgets, Trigger, WorkError, WorkerKind,
};
use rustykrab_core::ToolErrorKind;

use super::*;
use crate::errors::{
    classify, fingerprint, BudgetKind, Context, FailureInput, GapKind, PolicyStop, VerifierVerdict,
};

fn at(n: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(1_790_000_000 + n, 0).unwrap()
}

fn classified(input: FailureInput) -> WorkError {
    classify(
        &input,
        &Context {
            tool: None,
            worker_kind: Some(WorkerKind::Local),
        },
    )
}

fn timeout() -> WorkError {
    classified(FailureInput::ToolResult {
        tool: "web_fetch".into(),
        kind: ToolErrorKind::Timeout,
        message: "timed out after 30s".into(),
    })
}

fn check_failed() -> WorkError {
    classified(FailureInput::Verifier {
        verdict: VerifierVerdict::CheckFailed,
        detail: "cargo test: 3 failed".into(),
    })
}

fn unknown(message: &str) -> WorkError {
    classified(FailureInput::Raw {
        message: message.into(),
    })
}

fn gap(kind: GapKind, name: &str) -> WorkError {
    classified(FailureInput::CapabilityGap {
        gap: kind,
        name: name.into(),
    })
}

/// A bare error of `subclass`, for sweeping every subclass.
fn of(subclass: ErrorSubclass) -> WorkError {
    WorkError {
        class: subclass.class(),
        subclass,
        fingerprint: fingerprint(subclass.class(), subclass, None, None, "d"),
        detail: "d".into(),
        artifact_refs: vec![],
        observed_by: "test".into(),
    }
}

fn budgets(retries: u32, repairs: u32, worker_switches: u32) -> RungBudgets {
    RungBudgets {
        retries,
        repairs,
        worker_switches,
        ..RungBudgets::default()
    }
}

/// Climb until a move that ends the item's own ladder, recording each rung
/// with the same error.
fn climb(state: &mut LadderState, ctx: &LadderContext) -> Vec<Decision> {
    let mut moves = Vec::new();
    for i in 0..50 {
        let d = next(state, ctx);
        let ends = matches!(
            d,
            Decision::Surface(_) | Decision::Park(_) | Decision::PlanB | Decision::Replan
        );
        record(state, d.rung(), ctx.error.clone(), "failed again", at(i));
        moves.push(d);
        if ends {
            return moves;
        }
    }
    panic!("the ladder did not end: {moves:?}");
}

fn rungs(moves: &[Decision]) -> Vec<Rung> {
    moves.iter().map(Decision::rung).collect()
}

fn surfaced(d: &Decision) -> &Surfacing {
    match d {
        Decision::Surface(s) => s,
        other => panic!("expected a surface, got {other:?}"),
    }
}

#[test]
fn transient_errors_retry_then_repair_then_switch_then_surface() {
    let err = timeout();
    let mut state = LadderState::new(RungBudgets::default());
    let moves = climb(&mut state, &LadderContext::new(&err));
    assert_eq!(
        rungs(&moves),
        vec![
            Rung::Retry,
            Rung::Retry,
            Rung::Repair,
            Rung::Repair,
            Rung::SwitchWorker,
            Rung::Surface,
        ]
    );
    // The first repair re-briefs; the second, on the same failure, diagnoses.
    assert_eq!(
        moves[2],
        Decision::Repair {
            with_diagnosis: false
        }
    );
    assert_eq!(
        moves[3],
        Decision::Repair {
            with_diagnosis: true
        }
    );
    assert_eq!(surfaced(&moves[5]).reason, SurfaceReason::LadderSpent);
}

#[test]
fn only_transient_subclasses_retry() {
    use ErrorSubclass::*;
    for s in [Timeout, Network, UpstreamError] {
        let err = of(s);
        let d = next(&LadderState::default(), &LadderContext::new(&err));
        assert_eq!(d, Decision::Retry, "{s:?}");
    }
    for s in [
        InvalidArgs,
        NotFound,
        Format,
        Refusal,
        HallucinatedTool,
        Loop,
        Empty,
        Disk,
        Permission,
        Process,
        ClaimMismatch,
        CheckFailed,
        Incomplete,
    ] {
        let err = of(s);
        let d = next(&LadderState::default(), &LadderContext::new(&err));
        assert_eq!(
            d,
            Decision::Repair {
                with_diagnosis: false
            },
            "{s:?}"
        );
    }
    let err = check_failed();
    let moves = climb(&mut LadderState::default(), &LadderContext::new(&err));
    assert!(!rungs(&moves).contains(&Rung::Retry));
}

#[test]
fn budgets_exhaust_in_order_and_a_spent_rung_is_skipped() {
    let err = timeout();
    let ctx = LadderContext::new(&err);

    let mut state = LadderState::new(budgets(1, 1, 1));
    let moves = climb(&mut state, &ctx);
    assert_eq!(
        rungs(&moves),
        vec![Rung::Retry, Rung::Repair, Rung::SwitchWorker, Rung::Surface]
    );
    assert_eq!(state.used(Rung::Retry), 1);
    assert_eq!(state.left(Rung::Retry), 0);
    assert_eq!(state.left(Rung::PlanB), u32::MAX);

    // No retries: a transient error goes straight to repair.
    let mut state = LadderState::new(budgets(0, 1, 0));
    assert_eq!(
        rungs(&climb(&mut state, &ctx)),
        vec![Rung::Repair, Rung::Surface]
    );

    // Nothing left at all: surface.
    let mut state = LadderState::new(budgets(0, 0, 0));
    assert_eq!(rungs(&climb(&mut state, &ctx)), vec![Rung::Surface]);

    // Rungs spent on an earlier error stay spent for a new one.
    let mut state = LadderState::new(budgets(2, 1, 1));
    record(&mut state, Rung::Repair, timeout(), "no change", at(0));
    let other = check_failed();
    assert_eq!(
        next(&state, &LadderContext::new(&other)),
        Decision::SwitchWorker
    );
}

#[test]
fn a_capability_gap_skips_orders_0_and_1_for_its_order_2_rung() {
    let decide = |err: &WorkError, tool_exists| {
        let ctx = LadderContext {
            tool_exists,
            ..LadderContext::new(err)
        };
        next(&LadderState::default(), &ctx)
    };
    let need = |gap, subject: &str, trigger| CapabilityNeed {
        gap,
        subject: subject.into(),
        trigger,
    };

    let tool = gap(GapKind::Tool, "pdf_render");
    assert_eq!(
        decide(&tool, Some(true)),
        Decision::Acquire(need(GapKind::Tool, "pdf_render", Trigger::Now))
    );
    assert_eq!(
        decide(&tool, None),
        Decision::Acquire(need(GapKind::Tool, "pdf_render", Trigger::Now))
    );
    assert_eq!(
        decide(&tool, Some(false)),
        Decision::Build(need(GapKind::Tool, "pdf_render", Trigger::Now))
    );
    assert_eq!(
        decide(&gap(GapKind::Credential, "carrier login"), None),
        Decision::Acquire(need(
            GapKind::Credential,
            "carrier login",
            Trigger::OnCredential("carrier login".into())
        ))
    );
    assert_eq!(
        decide(&gap(GapKind::Consent, "send the email"), None),
        Decision::Acquire(need(
            GapKind::Consent,
            "send the email",
            Trigger::OnAnswer("send the email".into())
        ))
    );
    for kind in [GapKind::Install, GapKind::Compute, GapKind::Knowledge] {
        assert_eq!(
            decide(&gap(kind, "x"), None),
            Decision::Acquire(need(kind, "x", Trigger::Now)),
            "{kind:?}"
        );
    }
    assert_eq!(
        decide(&gap(GapKind::Capacity, "context window"), None),
        Decision::Request(need(GapKind::Capacity, "context window", Trigger::Now))
    );
    // A missing binary found by exit code is an install.
    let missing = classified(FailureInput::ProcessExit {
        code: Some(127),
        stderr_tail: "jq".into(),
    });
    assert!(matches!(
        decide(&missing, None),
        Decision::Acquire(CapabilityNeed {
            gap: GapKind::Install,
            ..
        })
    ));
}

#[test]
fn one_capability_item_per_gap() {
    let carrier = gap(GapKind::Credential, "carrier login");
    let mut state = LadderState::new(RungBudgets {
        acquisitions: 2,
        ..RungBudgets::default()
    });
    let d = next(&state, &LadderContext::new(&carrier));
    assert!(matches!(d, Decision::Acquire(_)));
    record(&mut state, d.rung(), carrier.clone(), "filed #7", at(0));

    // The same gap is not filed twice, whatever budget is left.
    let d = next(&state, &LadderContext::new(&carrier));
    assert!(matches!(d, Decision::Surface(_)), "{d:?}");

    // A different gap on the same item still gets its own.
    let bank = gap(GapKind::Credential, "bank login");
    assert!(matches!(
        next(&state, &LadderContext::new(&bank)),
        Decision::Acquire(_)
    ));

    // And the per-item budget caps them.
    record(&mut state, Rung::Acquire, bank.clone(), "filed #8", at(1));
    let third = gap(GapKind::Credential, "mail login");
    assert!(matches!(
        next(&state, &LadderContext::new(&third)),
        Decision::Surface(_)
    ));
}

#[test]
fn a_failed_load_builds_once_the_host_confirms_the_tool_is_missing() {
    let err = gap(GapKind::Tool, "pdf_render");
    let mut state = LadderState::default();
    let d = next(&state, &LadderContext::new(&err));
    assert!(matches!(d, Decision::Acquire(_)));
    record(&mut state, d.rung(), err.clone(), "not registered", at(0));

    let confirmed = LadderContext {
        tool_exists: Some(false),
        ..LadderContext::new(&err)
    };
    let d = next(&state, &confirmed);
    assert!(matches!(d, Decision::Build(_)), "{d:?}");
    record(&mut state, d.rung(), err.clone(), "build failed", at(1));
    assert!(matches!(next(&state, &confirmed), Decision::Surface(_)));
}

#[test]
fn recurrence_promotes_to_improve_at_the_threshold_then_continues() {
    let err = check_failed();
    let at_count = |n| LadderContext {
        recurrence_count: n,
        ..LadderContext::new(&err)
    };
    let mut state = LadderState::default();
    assert!(matches!(
        next(&state, &at_count(2)),
        Decision::Repair { .. }
    ));
    let d = next(&state, &at_count(3));
    assert_eq!(
        d,
        Decision::Improve {
            fingerprint: err.fingerprint.clone()
        }
    );
    record(&mut state, d.rung(), err.clone(), "filed #12", at(0));
    // After the improvement, the rung it would otherwise have chosen.
    assert_eq!(
        next(&state, &at_count(4)),
        Decision::Repair {
            with_diagnosis: false
        }
    );

    // A higher threshold holds the promotion back.
    let patient = LadderContext {
        promote_threshold: 5,
        ..at_count(3)
    };
    assert!(matches!(
        next(&LadderState::default(), &patient),
        Decision::Repair { .. }
    ));
}

#[test]
fn an_unknown_error_improves_at_once_then_repairs_with_diagnosis() {
    let err = unknown("flux capacitor desynchronised");
    let mut state = LadderState::default();
    let d = next(&state, &LadderContext::new(&err));
    assert_eq!(
        d,
        Decision::Improve {
            fingerprint: err.fingerprint.clone()
        }
    );
    record(&mut state, d.rung(), err.clone(), "filed #13", at(0));
    assert_eq!(
        next(&state, &LadderContext::new(&err)),
        Decision::Repair {
            with_diagnosis: true
        }
    );
}

#[test]
fn improvement_is_rate_limited_per_item() {
    let first = unknown("flux capacitor desynchronised");
    let second = unknown("the frobnicator sulked");
    assert_ne!(first.fingerprint, second.fingerprint);
    let mut state = LadderState::default();
    record(&mut state, Rung::Improve, first, "filed #13", at(0));
    // The default budget is one improvement per item.
    assert_eq!(
        next(&state, &LadderContext::new(&second)),
        Decision::Repair {
            with_diagnosis: true
        }
    );
    let none = LadderState::new(RungBudgets {
        improvements: 0,
        ..RungBudgets::default()
    });
    assert!(matches!(
        next(&none, &LadderContext::new(&second)),
        Decision::Repair { .. }
    ));
}

#[test]
fn section_6_4_order_on_one_item_with_a_failing_history() {
    // Small budgets so each rung runs once.
    let mut state = LadderState::new(RungBudgets {
        retries: 1,
        repairs: 1,
        worker_switches: 1,
        ..RungBudgets::default()
    });
    let slow = timeout();
    let failing = check_failed();
    let missing = gap(GapKind::Tool, "pdf_render");

    let mut moves = Vec::new();
    let mut step = |state: &mut LadderState, ctx: LadderContext| {
        let d = next(state, &ctx);
        record(
            state,
            d.rung(),
            ctx.error.clone(),
            "failed",
            at(moves.len() as i64),
        );
        moves.push(d.clone());
        d
    };
    let base = |err| LadderContext {
        has_plan_b: true,
        replans_left_on_parent: 1,
        tool_exists: Some(false),
        ..LadderContext::new(err)
    };

    // Order 0, then order 1 twice, on errors that change as the item runs.
    step(&mut state, base(&slow));
    step(&mut state, base(&failing));
    step(
        &mut state,
        LadderContext {
            recurrence_count: 2,
            ..base(&failing)
        },
    );
    // Order 2b: the tool does not exist.
    step(&mut state, base(&missing));
    // The build failed; the gap has now recurred to the threshold: order 3.
    step(
        &mut state,
        LadderContext {
            recurrence_count: 3,
            ..base(&missing)
        },
    );
    // Every own rung is spent: plan B, then (plan B failed) the re-plan.
    step(
        &mut state,
        LadderContext {
            recurrence_count: 3,
            ..base(&missing)
        },
    );
    step(
        &mut state,
        LadderContext {
            recurrence_count: 3,
            has_plan_b: false,
            ..base(&missing)
        },
    );
    let last = step(
        &mut state,
        LadderContext {
            recurrence_count: 3,
            has_plan_b: false,
            replans_left_on_parent: 0,
            ..base(&missing)
        },
    );

    assert_eq!(
        rungs(&moves),
        vec![
            Rung::Retry,
            Rung::Repair,
            Rung::SwitchWorker,
            Rung::Build,
            Rung::Improve,
            Rung::PlanB,
            Rung::Replan,
            Rung::Surface,
        ]
    );
    let order: Vec<u8> = moves.iter().map(|d| rank(d.rung())).collect();
    assert!(order.windows(2).all(|w| w[0] <= w[1]), "{order:?}");
    let s = surfaced(&last);
    assert_eq!(s.rungs.len(), 7);
    assert_eq!(s.reached, Some(Rung::Replan));
    assert_eq!(s.order_reached(), "3+");
}

#[test]
fn a_policy_stop_surfaces_at_once() {
    let scope = classified(FailureInput::Policy {
        stop: PolicyStop::Scope,
        detail: "writes outside the worktree".into(),
    });
    let everything_else = LadderContext {
        has_plan_b: true,
        replans_left_on_parent: 1,
        recurrence_count: 10,
        only_user_can_meet: true,
        ..LadderContext::new(&scope)
    };
    let d = next(&LadderState::default(), &everything_else);
    let s = surfaced(&d);
    assert_eq!(s.reason, SurfaceReason::PolicyStop);
    assert_eq!(s.reached, None);
    assert_eq!(s.order_reached(), "none");
    assert!(s.ask.contains("policy/scope"), "{}", s.ask);
    assert!(s.ask.contains("writes outside the worktree"), "{}", s.ask);

    // A policy stop flagged by the controller on any error does the same,
    // mid-ladder.
    let err = timeout();
    let mut state = LadderState::default();
    record(
        &mut state,
        Rung::Retry,
        err.clone(),
        "timed out again",
        at(0),
    );
    let stopped = LadderContext {
        policy_stop: true,
        has_plan_b: true,
        replans_left_on_parent: 1,
        ..LadderContext::new(&err)
    };
    let d = next(&state, &stopped);
    let s = surfaced(&d);
    assert_eq!(s.reason, SurfaceReason::PolicyStop);
    assert_eq!(s.order_reached(), "0");
}

#[test]
fn a_need_only_the_user_can_meet_parks_without_plan_b_and_takes_plan_b_with_one() {
    let cases = [
        (GapKind::Credential, BlockedReason::NeedsCredential),
        (GapKind::Consent, BlockedReason::NeedsConsent),
        (GapKind::Tool, BlockedReason::NeedsTool),
        (GapKind::Knowledge, BlockedReason::NeedsDecision),
        (GapKind::Capacity, BlockedReason::NeedsDecision),
    ];
    for (kind, reason) in cases {
        let err = gap(kind, "x");
        let only_user = LadderContext {
            only_user_can_meet: true,
            // Not re-planned around, even with a re-plan left.
            replans_left_on_parent: 1,
            ..LadderContext::new(&err)
        };
        assert_eq!(
            next(&LadderState::default(), &only_user),
            Decision::Park(reason),
            "{kind:?}"
        );
        let with_plan_b = LadderContext {
            has_plan_b: true,
            ..only_user
        };
        assert_eq!(
            next(&LadderState::default(), &with_plan_b),
            Decision::PlanB,
            "{kind:?}"
        );
    }

    // A recurring need still files its improvement first.
    let err = gap(GapKind::Credential, "carrier login");
    let mut state = LadderState::default();
    let ctx = LadderContext {
        only_user_can_meet: true,
        recurrence_count: 3,
        ..LadderContext::new(&err)
    };
    let d = next(&state, &ctx);
    assert!(matches!(d, Decision::Improve { .. }));
    record(&mut state, d.rung(), err.clone(), "filed #14", at(0));
    assert_eq!(
        next(&state, &ctx),
        Decision::Park(BlockedReason::NeedsCredential)
    );
}

#[test]
fn inside_a_graph_plan_b_then_replan_then_surface() {
    let err = check_failed();
    let mut state = LadderState::new(budgets(0, 0, 0));
    let ctx = LadderContext {
        has_plan_b: true,
        replans_left_on_parent: 1,
        ..LadderContext::new(&err)
    };
    assert_eq!(next(&state, &ctx), Decision::PlanB);
    record(&mut state, Rung::PlanB, err.clone(), "plan B failed", at(0));
    // A plan B is released once, even if the controller still reports one.
    assert_eq!(next(&state, &ctx), Decision::Replan);
    let spent = LadderContext {
        replans_left_on_parent: 0,
        ..ctx
    };
    assert!(matches!(next(&state, &spent), Decision::Surface(_)));
}

#[test]
fn a_spent_budget_skips_the_own_rungs() {
    let err = classified(FailureInput::BudgetExhausted {
        budget: BudgetKind::Iterations,
        detail: "25 of 25".into(),
    });
    let d = next(&LadderState::default(), &LadderContext::new(&err));
    assert!(matches!(d, Decision::Surface(_)), "{d:?}");
    let with_plan_b = LadderContext {
        has_plan_b: true,
        ..LadderContext::new(&err)
    };
    assert_eq!(next(&LadderState::default(), &with_plan_b), Decision::PlanB);
}

#[test]
fn surfacing_carries_the_ladder() {
    let err = timeout();
    let mut state = LadderState::new(budgets(2, 1, 1));
    let moves = climb(&mut state, &LadderContext::new(&err));
    let s = surfaced(moves.last().unwrap());
    assert_eq!(s.error, err);
    // Everything climbed before surfacing.
    assert_eq!(s.rungs, state.history[..state.history.len() - 1].to_vec());
    assert_eq!(s.reached, Some(Rung::SwitchWorker));
    assert_eq!(s.order_reached(), "1");
    assert!(s.ask.contains("tool/timeout"), "{}", s.ask);
    assert!(
        s.ask.contains("web_fetch: timed out after 30s"),
        "{}",
        s.ask
    );
    assert_eq!(
        s.summary(),
        "Order 0: 2 retries on tool/timeout, last: failed again. \
         Order 1: 1 repair on tool/timeout, last: failed again; \
         1 worker switch on tool/timeout, last: failed again. \
         Reached order 1."
    );
}

#[test]
fn the_summary_says_what_was_tried_at_each_order() {
    assert_eq!(
        summary(&LadderState::default()),
        "Nothing tried: the ladder was not climbed."
    );
    let mut state = LadderState::default();
    record(
        &mut state,
        Rung::Retry,
        timeout(),
        "timed out again.",
        at(0),
    );
    record(
        &mut state,
        Rung::Repair,
        check_failed(),
        "same check fails",
        at(1),
    );
    record(
        &mut state,
        Rung::Acquire,
        gap(GapKind::Credential, "carrier login"),
        "only the user holds it",
        at(2),
    );
    record(
        &mut state,
        Rung::Improve,
        unknown("flux capacitor desynchronised"),
        "",
        at(3),
    );
    record(
        &mut state,
        Rung::PlanB,
        check_failed(),
        "plan B #f running on pinch",
        at(4),
    );
    assert_eq!(
        summary(&state),
        "Order 0: 1 retry on tool/timeout, last: timed out again. \
         Order 1: 1 repair on verification/check_failed, last: same check fails. \
         Order 2a: 1 acquisition for credential: carrier login, last: only the user holds it. \
         Order 3: 1 internal item on unknown/unclassified. \
         Order 3+: 1 plan B on verification/check_failed, last: plan B #f running on pinch. \
         Reached order 3+."
    );
    assert_eq!(state.order_reached(), "3+");
}

#[test]
fn every_decision_is_reachable() {
    fn name(d: &Decision) -> &'static str {
        match d {
            Decision::Retry => "retry",
            Decision::Repair { .. } => "repair",
            Decision::SwitchWorker => "switch_worker",
            Decision::Acquire(_) => "acquire",
            Decision::Build(_) => "build",
            Decision::Request(_) => "request",
            Decision::Improve { .. } => "improve",
            Decision::PlanB => "plan_b",
            Decision::Replan => "replan",
            Decision::Surface(_) => "surface",
            Decision::Park(_) => "park",
        }
    }
    let mut seen = Vec::new();
    let mut see = |d: Decision| seen.push(name(&d));

    let slow = timeout();
    let failing = check_failed();
    let fresh = LadderState::default();
    see(next(&fresh, &LadderContext::new(&slow)));
    see(next(&fresh, &LadderContext::new(&failing)));
    see(next(
        &LadderState::new(budgets(0, 0, 1)),
        &LadderContext::new(&failing),
    ));
    let tool = gap(GapKind::Tool, "pdf_render");
    see(next(&fresh, &LadderContext::new(&tool)));
    see(next(
        &fresh,
        &LadderContext {
            tool_exists: Some(false),
            ..LadderContext::new(&tool)
        },
    ));
    let capacity = gap(GapKind::Capacity, "context window");
    see(next(&fresh, &LadderContext::new(&capacity)));
    see(next(&fresh, &LadderContext::new(&unknown("???"))));
    let spent = LadderState::new(budgets(0, 0, 0));
    see(next(
        &spent,
        &LadderContext {
            has_plan_b: true,
            ..LadderContext::new(&failing)
        },
    ));
    see(next(
        &spent,
        &LadderContext {
            replans_left_on_parent: 1,
            ..LadderContext::new(&failing)
        },
    ));
    see(next(&spent, &LadderContext::new(&failing)));
    see(next(
        &fresh,
        &LadderContext {
            only_user_can_meet: true,
            ..LadderContext::new(&gap(GapKind::Credential, "carrier login"))
        },
    ));

    seen.sort_unstable();
    seen.dedup();
    assert_eq!(seen.len(), 11, "{seen:?}");
}

#[test]
fn a_decision_names_the_rung_to_record() {
    assert_eq!(Decision::Retry.rung(), Rung::Retry);
    assert_eq!(
        Decision::Park(BlockedReason::NeedsConsent).rung(),
        Rung::Surface
    );
    assert_eq!(
        Decision::Improve {
            fingerprint: "f".into()
        }
        .rung(),
        Rung::Improve
    );
    for (i, r) in RUNGS.iter().enumerate() {
        assert_eq!(usize::from(rank(*r)), i);
    }
}

#[test]
fn scenario_13_a_missing_tool_is_built_and_the_user_is_never_asked() {
    let err = gap(GapKind::Tool, "pdf_render");
    let ctx = LadderContext {
        tool_exists: Some(false),
        ..LadderContext::new(&err)
    };
    let d = next(&LadderState::default(), &ctx);
    assert_eq!(
        d,
        Decision::Build(CapabilityNeed {
            gap: GapKind::Tool,
            subject: "pdf_render".into(),
            trigger: Trigger::Now,
        })
    );
}

#[test]
fn scenario_14_an_unknown_failure_files_its_internal_item_first() {
    let err = unknown("flux capacitor desynchronised");
    let d = next(&LadderState::default(), &LadderContext::new(&err));
    let Decision::Improve { fingerprint } = &d else {
        panic!("expected improve, got {d:?}");
    };
    assert_eq!(fingerprint, &err.fingerprint);
    let draft = crate::errors::internal_item_draft(&err, vec![]);
    assert!(draft.title.contains(fingerprint.as_str()));
}

#[test]
fn scenario_20_the_failing_middle_item_climbs_its_own_ladder_before_anything_moves() {
    // b in a chain under P: no plan B, P has its one re-plan.
    let err = timeout();
    let mut state = LadderState::default();
    let ctx = LadderContext {
        replans_left_on_parent: 1,
        ..LadderContext::new(&err)
    };
    let moves = climb(&mut state, &ctx);
    assert_eq!(
        rungs(&moves),
        vec![
            Rung::Retry,
            Rung::Retry,
            Rung::Repair,
            Rung::Repair,
            Rung::SwitchWorker,
            Rung::Replan,
        ]
    );
    // Every move before the graph move is b's own: nothing downstream moves
    // while it runs.
    assert!(moves[..5]
        .iter()
        .all(|d| rank(d.rung()) < rank(Rung::PlanB)));
    // The re-plan spent, P surfaces once, naming the rung b reached.
    let d = next(
        &state,
        &LadderContext {
            replans_left_on_parent: 0,
            ..ctx
        },
    );
    assert_eq!(surfaced(&d).order_reached(), "3+");
}

#[test]
fn scenario_21_plan_b_runs_before_anything_is_surfaced() {
    let err = check_failed();
    let mut state = LadderState::default();
    let ctx = LadderContext {
        has_plan_b: true,
        ..LadderContext::new(&err)
    };
    let moves = climb(&mut state, &ctx);
    assert_eq!(moves.last(), Some(&Decision::PlanB));
    assert!(!moves.iter().any(|d| matches!(d, Decision::Surface(_))));
}
