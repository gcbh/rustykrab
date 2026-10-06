//! The question router (plan section 7): code first, the model only asks.
//!
//! [`route`] classifies one question from what code can check (the item's
//! state, what was already answered, the standing judgment in force, the
//! question's own kind and options) and, only where code has nothing to
//! say, the class the asking model claimed. First match wins:
//!
//! | Rule | Class | Route |
//! |---|---|---|
//! | `item_closed`: the item is no longer open | obsolete | close |
//! | `repeat`: the item already had this question answered | (as before) | the worker ignored an answer: a failure for the ladder |
//! | `reserved`: a grant reserves the topic for the user | blocking now | ask |
//! | `credential`: only the user holds a credential | blocking now | ask |
//! | `delegated_resource`: a consent question about a delegated resource | delegated | yes, by policy |
//! | `delegated_topic`: a grant lets policy decide the topic, and there is a choice to make | delegated | the default, else the first option |
//! | `defaultable`: a recorded default, one option only, or a claimed default with options | defaultable | the default |
//! | `researchable`: claimed, and not a consent | researchable | file a `research` item |
//! | `blocking_later`: claimed | blocking later | record for the next message |
//! | `obsolete`: claimed | obsolete | close |
//! | `blocking_now`: everything else | blocking now | ask |
//!
//! A model may not file `needs_decision` for a defaultable question: a
//! question the router can answer never reaches the user, whatever the
//! model called it. Nor may a model claim `delegated`: authority comes
//! from a grant, never from the asker (the projects crate's rule, "must
//! never infer authority absent from them").

use rustykrab_core::questions::{DelegatedDecision, QuestionClass, QuestionKind};

use super::judgment::Judgment;

/// A question as it was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asked {
    pub text: String,
    /// The class the asking model claimed, if it named one the router
    /// knows.
    pub claimed: Option<QuestionClass>,
    pub kind: QuestionKind,
    pub options: Vec<String>,
    /// A recorded default the asker named.
    pub default: Option<String>,
}

/// What the router knows beyond the question.
#[derive(Debug, Clone, Copy)]
pub struct RouteContext<'a> {
    /// The asking item is still open.
    pub item_open: bool,
    /// `(question id, answer)` of an earlier question on the same item with
    /// the same text, already settled with an answer.
    pub answered_before: Option<(&'a str, &'a str)>,
    pub judgment: &'a Judgment,
}

/// Where a question goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Blocking now: to the user, at once, and the item parks.
    Ask,
    /// Blocking later: recorded and carried in the parent's next message.
    Later,
    /// Researchable: a `research` item finds the answer.
    Research,
    /// Defaultable: answered with the recorded default.
    Default { answer: String },
    /// Delegated: decided by standing judgment, with the record and the
    /// grant that covered it.
    Delegated {
        answer: String,
        decision: DelegatedDecision,
        policy: String,
    },
    /// Obsolete: closed.
    Obsolete { why: String },
    /// The item already had this question answered: the worker did not
    /// use the answer, which is a failure for the ladder, not a question.
    Repeat { previous: String },
}

/// The router's verdict: the class, where it goes, and the rule that
/// decided, so the record says why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Routed {
    pub class: QuestionClass,
    pub route: Route,
    pub rule: &'static str,
}

fn routed(class: QuestionClass, route: Route, rule: &'static str) -> Routed {
    Routed { class, route, rule }
}

/// Classify one question (section 7).
pub fn route(asked: &Asked, ctx: &RouteContext<'_>) -> Routed {
    use QuestionClass as C;
    if !ctx.item_open {
        return routed(
            C::Obsolete,
            Route::Obsolete {
                why: "the item that asked is closed".to_string(),
            },
            "item_closed",
        );
    }
    if let Some((previous, _)) = ctx.answered_before {
        return routed(
            asked.claimed.unwrap_or(C::BlockingNow),
            Route::Repeat {
                previous: previous.to_string(),
            },
            "repeat",
        );
    }
    if ctx.judgment.reserves(&asked.text).is_some() {
        return routed(C::BlockingNow, Route::Ask, "reserved");
    }
    if asked.kind == QuestionKind::Credential {
        return routed(C::BlockingNow, Route::Ask, "credential");
    }
    if asked.kind == QuestionKind::Consent {
        if let Some((resource, policy)) = ctx.judgment.delegates_in(&asked.text) {
            let decision = DelegatedDecision {
                chosen: "yes".to_string(),
                alternatives: vec!["no".to_string(), "ask the user".to_string()],
                rationale: format!("the question is about {resource}, which the user delegated"),
                authority: format!("standing judgment {policy}: writing {resource} is delegated"),
                revisit: format!(
                    "revoke standing judgment {policy} (`rustykrab judgment revoke {policy}`), \
                     and consent questions about {resource} go to you again"
                ),
            };
            return routed(
                C::Delegated,
                Route::Delegated {
                    answer: "yes".to_string(),
                    decision,
                    policy: policy.to_string(),
                },
                "delegated_resource",
            );
        }
    }
    let choice = asked
        .default
        .clone()
        .or_else(|| asked.options.first().cloned());
    if let (Some((topic, policy)), Some(answer)) = (ctx.judgment.decides(&asked.text), &choice) {
        if asked.kind != QuestionKind::Consent {
            let alternatives: Vec<String> = asked
                .options
                .iter()
                .filter(|o| *o != answer)
                .cloned()
                .chain(std::iter::once("ask the user".to_string()))
                .collect();
            let decision = DelegatedDecision {
                chosen: answer.clone(),
                alternatives,
                rationale: match &asked.default {
                    Some(d) => format!("the asker's recorded default is {d}"),
                    None => "the first option the asker offered".to_string(),
                },
                authority: format!(
                    "standing judgment {policy}: questions about {topic} are decided by policy"
                ),
                revisit: format!(
                    "answer differently with `rustykrab work answer`, or revoke standing \
                     judgment {policy} (`rustykrab judgment revoke {policy}`)"
                ),
            };
            return routed(
                C::Delegated,
                Route::Delegated {
                    answer: answer.clone(),
                    decision,
                    policy: policy.to_string(),
                },
                "delegated_topic",
            );
        }
    }
    let defaultable = asked.default.is_some()
        || (asked.options.len() == 1 && asked.kind != QuestionKind::Consent)
        || (asked.claimed == Some(C::Defaultable) && !asked.options.is_empty());
    if defaultable && asked.kind != QuestionKind::Consent {
        if let Some(answer) = choice {
            return routed(C::Defaultable, Route::Default { answer }, "defaultable");
        }
    }
    match asked.claimed {
        Some(C::Researchable) if asked.kind == QuestionKind::Decision => {
            routed(C::Researchable, Route::Research, "researchable")
        }
        Some(C::BlockingLater) => routed(C::BlockingLater, Route::Later, "blocking_later"),
        Some(C::Obsolete) => routed(
            C::Obsolete,
            Route::Obsolete {
                why: "the asker said it no longer needs an answer".to_string(),
            },
            "obsolete",
        ),
        _ => routed(C::BlockingNow, Route::Ask, "blocking_now"),
    }
}

/// The default a question's own words record ("... the recorded default is
/// 9am"), when it names one of the options. Kept on the question's row for
/// the evaluation of section 10 (a surfaced question answered with its
/// recorded default was an avoidable escalation), and never used to route:
/// the router trusts only a default the asker gave as data.
pub fn stated_default(text: &str, options: &[String]) -> Option<String> {
    let lower = text.to_lowercase();
    let at = lower.find("default is ")? + "default is ".len();
    let said: String = lower[at..]
        .chars()
        .take_while(|c| !matches!(c, '.' | ',' | ';' | '?' | '!' | '\n'))
        .collect();
    let said = said.trim();
    options
        .iter()
        .find(|o| o.trim().to_lowercase() == said)
        .cloned()
}

/// Normalised question text, for recognising the same question asked
/// again: case, spacing and trailing punctuation do not count.
pub fn same_question(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        s.to_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .trim_end_matches(['?', '.', '!', ' '])
            .to_string()
    };
    norm(a) == norm(b)
}
