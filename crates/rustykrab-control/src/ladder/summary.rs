//! The text a surfaced message carries: what was tried at each order
//! (section 8, "Surfacing carries the ladder") and what is being asked.

use rustykrab_core::work::{Rung, RungEvent, WorkError};

use super::{LadderState, SurfaceReason, RUNGS};
use crate::errors::gap_of;

/// One paragraph: for each order climbed, how many times each rung ran, on
/// which errors, and its last outcome; then the order reached. For example:
///
/// ```text
/// Order 0: 2 retries on tool/timeout, last: timed out again. Order 1:
/// 1 repair on tool/timeout, last: same timeout. Reached order 1.
/// ```
pub fn summary(state: &LadderState) -> String {
    summarise(&state.history)
}

pub(super) fn summarise(history: &[RungEvent]) -> String {
    if history.is_empty() {
        return "Nothing tried: the ladder was not climbed.".to_string();
    }
    let mut orders: Vec<&'static str> = Vec::new();
    for rung in RUNGS {
        if history.iter().any(|e| e.rung == rung) && !orders.contains(&rung.order()) {
            orders.push(rung.order());
        }
    }
    let mut sentences: Vec<String> = orders
        .iter()
        .map(|order| {
            let clauses: Vec<String> = RUNGS
                .iter()
                .filter(|r| r.order() == *order)
                .filter_map(|r| clause(*r, history))
                .collect();
            format!("Order {order}: {}.", clauses.join("; "))
        })
        .collect();
    let reached = history
        .iter()
        .map(|e| e.rung)
        .max_by_key(|r| super::rank(*r))
        .map_or("none", |r| r.order());
    sentences.push(format!("Reached order {reached}."));
    sentences.join(" ")
}

/// `2 retries on tool/timeout, last: timed out again`, or `None` when the
/// rung was not climbed.
fn clause(rung: Rung, history: &[RungEvent]) -> Option<String> {
    let events: Vec<&RungEvent> = history.iter().filter(|e| e.rung == rung).collect();
    let last = events.last()?;
    let n = events.len();
    let (one, many) = noun(rung);
    let mut text = format!("{n} {}", if n == 1 { one } else { many });
    let mut labels: Vec<String> = Vec::new();
    for e in &events {
        if let Some(err) = &e.error {
            let label = label(rung, err);
            if !labels.contains(&label) {
                labels.push(label);
            }
        }
    }
    if !labels.is_empty() {
        let joiner = if is_capability(rung) { " for " } else { " on " };
        text.push_str(joiner);
        text.push_str(&labels.join(", "));
    }
    let outcome = last.outcome.trim().trim_end_matches('.');
    if !outcome.is_empty() {
        text.push_str(", last: ");
        text.push_str(outcome);
    }
    Some(text)
}

fn is_capability(rung: Rung) -> bool {
    matches!(rung, Rung::Acquire | Rung::Build | Rung::Request)
}

/// A capability rung names the gap; every other rung names the error class.
fn label(rung: Rung, err: &WorkError) -> String {
    if is_capability(rung) {
        if let Some(gap) = gap_of(err) {
            return format!("{}: {}", gap.kind.as_str(), gap.subject);
        }
    }
    format!("{}/{}", err.class.as_str(), err.subclass.as_str())
}

fn noun(rung: Rung) -> (&'static str, &'static str) {
    match rung {
        Rung::Retry => ("retry", "retries"),
        Rung::Repair => ("repair", "repairs"),
        Rung::SwitchWorker => ("worker switch", "worker switches"),
        Rung::Acquire => ("acquisition", "acquisitions"),
        Rung::Build => ("build", "builds"),
        Rung::Request => ("capability request", "capability requests"),
        Rung::Improve => ("internal item", "internal items"),
        Rung::PlanB => ("plan B", "plan Bs"),
        Rung::Replan => ("re-plan", "re-plans"),
        Rung::Surface => ("question", "questions"),
    }
}

/// What the surfaced message asks.
pub(super) fn ask(reason: SurfaceReason, err: &WorkError) -> String {
    let what = format!("{}/{}", err.class.as_str(), err.subclass.as_str());
    match reason {
        SurfaceReason::PolicyStop => format!(
            "Stopped by policy ({what}): {}. Allow it, change it, or cancel it?",
            err.detail
        ),
        SurfaceReason::LadderSpent => format!(
            "Every rung is spent ({what}): {}. How should it go on, or should it stop?",
            err.detail
        ),
    }
}
