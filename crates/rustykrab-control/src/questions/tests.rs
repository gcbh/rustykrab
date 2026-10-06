use chrono::Utc;
use rustykrab_core::questions::{JudgmentCheck, QuestionClass, QuestionKind};
use rustykrab_store::JudgmentRow;

use super::*;

fn asked(text: &str, claimed: Option<QuestionClass>, options: &[&str]) -> Asked {
    Asked {
        text: text.to_string(),
        claimed,
        kind: QuestionKind::Decision,
        options: options.iter().map(|o| o.to_string()).collect(),
        default: None,
    }
}

fn ctx(judgment: &Judgment) -> RouteContext<'_> {
    RouteContext {
        item_open: true,
        answered_before: None,
        judgment,
    }
}

fn grant(id: &str, text: &str) -> JudgmentRow {
    let compiled = compile(text);
    JudgmentRow {
        id: id.to_string(),
        scope: "all".to_string(),
        text: text.to_string(),
        checks: compiled.checks,
        policy: compiled.policy,
        unrecognised: compiled.unrecognised,
        granted_by: Some("user".to_string()),
        granted_at: Utc::now(),
        revoked_at: None,
    }
}

// ── the router ─────────────────────────────────────────────────────────

#[test]
fn a_choice_between_two_options_is_blocking_now() {
    let j = Judgment::new(baseline());
    let r = route(
        &asked(
            "Which florist, Petals or Stems?",
            Some(QuestionClass::BlockingNow),
            &["Petals", "Stems"],
        ),
        &ctx(&j),
    );
    assert_eq!(r.class, QuestionClass::BlockingNow);
    assert_eq!(r.route, Route::Ask);
}

#[test]
fn a_defaultable_question_is_answered_with_its_default_and_never_asked() {
    let j = Judgment::new(baseline());
    let r = route(
        &asked(
            "Should the reminder use the usual 9am?",
            Some(QuestionClass::Defaultable),
            &["9am"],
        ),
        &ctx(&j),
    );
    assert_eq!(r.class, QuestionClass::Defaultable);
    assert_eq!(
        r.route,
        Route::Default {
            answer: "9am".into()
        }
    );
}

#[test]
fn a_model_may_not_file_needs_decision_for_a_defaultable_question() {
    // Claimed blocking now, but there is only one possible answer.
    let j = Judgment::new(baseline());
    let r = route(
        &asked(
            "Use the only slot left, 10:30?",
            Some(QuestionClass::BlockingNow),
            &["10:30"],
        ),
        &ctx(&j),
    );
    assert_eq!(r.class, QuestionClass::Defaultable);
    // A recorded default the asker named wins over the options.
    let mut q = asked(
        "Reminder time?",
        Some(QuestionClass::BlockingNow),
        &["8am", "9am"],
    );
    q.default = Some("9am".into());
    assert_eq!(
        route(&q, &ctx(&j)).route,
        Route::Default {
            answer: "9am".into()
        }
    );
}

#[test]
fn a_claimed_default_with_nothing_to_default_to_is_asked() {
    let j = Judgment::new(baseline());
    let r = route(
        &asked(
            "What should I call it?",
            Some(QuestionClass::Defaultable),
            &[],
        ),
        &ctx(&j),
    );
    assert_eq!(r.route, Route::Ask);
}

#[test]
fn a_model_may_not_claim_delegated_authority() {
    let j = Judgment::new(baseline());
    let r = route(
        &asked(
            "Which restaurant?",
            Some(QuestionClass::Delegated),
            &["Tasca", "Cervejaria"],
        ),
        &ctx(&j),
    );
    assert_eq!(r.route, Route::Ask);
    assert_eq!(r.rule, "blocking_now");
}

#[test]
fn a_granted_topic_is_decided_by_policy_with_the_record() {
    let j = Judgment::from_grants(
        baseline(),
        &[grant("j1", "Use your judgment on restaurants.")],
    );
    let r = route(
        &asked(
            "Which restaurant for Friday?",
            Some(QuestionClass::BlockingNow),
            &["Tasca", "Cervejaria"],
        ),
        &ctx(&j),
    );
    assert_eq!(r.class, QuestionClass::Delegated);
    let Route::Delegated {
        answer,
        decision,
        policy,
    } = r.route
    else {
        panic!("not delegated");
    };
    assert_eq!(answer, "Tasca");
    assert_eq!(policy, "j1");
    assert!(decision.alternatives.contains(&"Cervejaria".to_string()));
    assert!(decision.authority.contains("j1"));
    assert!(decision.revisit.contains("revoke"));
    assert!(!decision.rationale.is_empty());
}

#[test]
fn a_reserved_topic_is_asked_even_when_it_has_a_default() {
    let j = Judgment::from_grants(
        baseline(),
        &[grant("j1", "Always ask me about anything medical.")],
    );
    let r = route(
        &asked(
            "Book the medical checkup at the usual clinic?",
            Some(QuestionClass::Defaultable),
            &["usual clinic"],
        ),
        &ctx(&j),
    );
    assert_eq!(r.rule, "reserved");
    assert_eq!(r.route, Route::Ask);
}

#[test]
fn research_later_and_obsolete_follow_the_claim() {
    let j = Judgment::new(baseline());
    let c = ctx(&j);
    assert_eq!(
        route(
            &asked(
                "When does the pharmacy open?",
                Some(QuestionClass::Researchable),
                &[]
            ),
            &c
        )
        .route,
        Route::Research
    );
    assert_eq!(
        route(
            &asked(
                "Hotel or flat in June?",
                Some(QuestionClass::BlockingLater),
                &[]
            ),
            &c
        )
        .route,
        Route::Later
    );
    assert!(matches!(
        route(
            &asked("Still need the form?", Some(QuestionClass::Obsolete), &[]),
            &c
        )
        .route,
        Route::Obsolete { .. }
    ));
}

#[test]
fn a_closed_item_makes_its_question_obsolete_and_a_repeat_is_a_failure() {
    let j = Judgment::new(baseline());
    let q = asked(
        "Which florist?",
        Some(QuestionClass::BlockingNow),
        &["A", "B"],
    );
    let closed = RouteContext {
        item_open: false,
        answered_before: None,
        judgment: &j,
    };
    assert_eq!(route(&q, &closed).rule, "item_closed");
    let again = RouteContext {
        item_open: true,
        answered_before: Some(("q1", "A")),
        judgment: &j,
    };
    assert_eq!(
        route(&q, &again).route,
        Route::Repeat {
            previous: "q1".into()
        }
    );
    assert!(same_question("Which florist?", "which  florist"));
    assert!(!same_question("Which florist?", "Which flowers?"));
}

#[test]
fn credentials_always_go_to_the_user_and_delegated_resources_consent_by_policy() {
    let j = Judgment::from_grants(
        baseline(),
        &[grant("j2", "You may change my calendar without asking.")],
    );
    let mut cred = asked("I need the carrier login", None, &[]);
    cred.kind = QuestionKind::Credential;
    assert_eq!(route(&cred, &ctx(&j)).rule, "credential");
    let mut consent = asked(
        "May I add the dentist to your calendar?",
        None,
        &["yes", "no"],
    );
    consent.kind = QuestionKind::Consent;
    let r = route(&consent, &ctx(&j));
    assert_eq!(r.rule, "delegated_resource");
    // A consent question is never defaulted: yes needs authority.
    let bare = Judgment::new(baseline());
    assert_eq!(route(&consent, &ctx(&bare)).route, Route::Ask);
}

#[test]
fn a_default_stated_in_words_is_recorded_but_does_not_route() {
    let options = vec!["9am".to_string(), "10am".to_string()];
    let text = "What time should the check-in run? The recorded default is 9am.";
    assert_eq!(stated_default(text, &options).as_deref(), Some("9am"));
    assert_eq!(stated_default("Default is noon.", &options), None);
    let j = Judgment::new(baseline());
    let q = asked(text, Some(QuestionClass::BlockingNow), &["9am", "10am"]);
    assert_eq!(route(&q, &ctx(&j)).route, Route::Ask, "words do not route");
}

// ── the compiler ───────────────────────────────────────────────────────

#[test]
fn grants_compile_to_checks_deterministically() {
    let text = "Ask me before paying for anything. Ask before messaging other people. \
                You may change my calendar without asking. Ask me about plans with more \
                than 6 steps. Plans over 50k tokens need my approval. Use your judgment on \
                restaurants. Always ask me about travel. Make it nice.";
    let a = compile(text);
    let b = compile(text);
    assert_eq!(a, b, "the same words compile to the same checks");
    assert_eq!(
        a.checks,
        vec![
            JudgmentCheck::ConsentFor {
                resource: PAYMENT.into()
            },
            JudgmentCheck::ConsentFor {
                resource: THIRD_PARTY_MESSAGE.into()
            },
            JudgmentCheck::Delegate {
                resource: "calendar".into()
            },
            JudgmentCheck::PlanItemsAtMost { items: 6 },
            JudgmentCheck::PlanTokensAtMost { tokens: 50_000 },
            JudgmentCheck::DecideOn {
                topic: "restaurants".into()
            },
            JudgmentCheck::ReserveOn {
                topic: "travel".into()
            },
        ]
    );
    assert_eq!(a.unrecognised, vec!["Make it nice".to_string()]);
    assert_eq!(a.policy.statement, text);
    assert!(!a.policy.delegated_scopes.is_empty());
    assert!(!a.policy.reserved_decisions.is_empty());
}

#[test]
fn grants_fold_over_the_baseline_and_later_ones_win() {
    let base = baseline();
    assert!(base.names_side_effect("message:third_party").is_some());
    assert!(base.names_side_effect("payment:card").is_some());
    assert!(base.names_side_effect("calendar").is_none());
    let j = Judgment::from_grants(
        base,
        &[
            grant("j1", "Ask me before changing my calendar."),
            grant("j2", "You may send messages to people without asking."),
            grant("j3", "Plans with more than 3 items need my approval."),
        ],
    );
    assert!(j.approval.names_side_effect("calendar").is_some());
    assert!(j
        .approval
        .names_side_effect("message:third_party")
        .is_none());
    assert_eq!(j.approval.max_items, Some(3));
    assert_eq!(j.approval.id.as_deref(), Some("j3"));
    assert_eq!(j.sources, vec!["j1", "j2", "j3"]);
    let mut revoked = grant("j4", "Plans with more than 9 items need my approval.");
    revoked.revoked_at = Some(Utc::now());
    let j = Judgment::from_grants(baseline(), &[revoked]);
    assert_eq!(
        j.approval.max_items,
        Some(10),
        "a revoked grant counts for nothing"
    );
    assert!(!j.describe().is_empty());
}

#[test]
fn classes_map_to_the_projects_vocabulary_and_back() {
    for class in QuestionClass::ALL {
        assert_eq!(class_of(&impact_of(class)), Some(class));
    }
    assert_eq!(
        class_of(&rustykrab_projects::QuestionImpact::Custom("x".into())),
        None
    );
}
