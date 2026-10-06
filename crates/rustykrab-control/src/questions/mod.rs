//! Questions, blocked states and standing judgment (plan section 7), as
//! pure functions: the router ([`route`]) and the standing-judgment
//! compiler ([`compile`], [`Judgment`]). The controller carries their
//! verdicts out (`controller/questions.rs`); nothing here does I/O.
//!
//! The class vocabulary is `rustykrab_core::questions`, which matches the
//! projects crate's `QuestionImpact` word for word; [`impact_of`] maps one
//! to the other, so a planning slice and a work item speak of a question
//! the same way.

mod judgment;
mod router;

#[cfg(test)]
mod tests;

pub use judgment::{baseline, compile, Compiled, Judgment, PAYMENT, THIRD_PARTY_MESSAGE};
pub use router::{route, same_question, stated_default, Asked, Route, RouteContext, Routed};

use rustykrab_core::questions::QuestionClass;
use rustykrab_projects::QuestionImpact;

/// The projects crate's name for a question class.
pub fn impact_of(class: QuestionClass) -> QuestionImpact {
    match class {
        QuestionClass::BlockingNow => QuestionImpact::BlockingNow,
        QuestionClass::BlockingLater => QuestionImpact::BlockingLater,
        QuestionClass::Researchable => QuestionImpact::Researchable,
        QuestionClass::Defaultable => QuestionImpact::Defaultable,
        QuestionClass::Delegated => QuestionImpact::Delegated,
        QuestionClass::Obsolete => QuestionImpact::Obsolete,
    }
}

/// A question class from the projects crate's name for it; a custom impact
/// is no class the router knows.
pub fn class_of(impact: &QuestionImpact) -> Option<QuestionClass> {
    Some(match impact {
        QuestionImpact::BlockingNow => QuestionClass::BlockingNow,
        QuestionImpact::BlockingLater => QuestionClass::BlockingLater,
        QuestionImpact::Researchable => QuestionClass::Researchable,
        QuestionImpact::Defaultable => QuestionClass::Defaultable,
        QuestionImpact::Delegated => QuestionClass::Delegated,
        QuestionImpact::Obsolete => QuestionClass::Obsolete,
        QuestionImpact::Custom(_) => return None,
    })
}
