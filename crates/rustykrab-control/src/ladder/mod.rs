//! The resolution ladder (plan sections 8 and 6.4) as a pure state machine.
//!
//! [`next`] reads an item's rung history and budgets ([`LadderState`]) and
//! the classified failure with what the controller knows about the item's
//! place in its graph ([`LadderContext`]), and returns the next move
//! ([`Decision`]). The controller carries the move out and writes it back
//! with [`record`]. Nothing here does I/O, so the same history always yields
//! the same move and a restart re-derives it.
//!
//! The order of the checks in [`next`]:
//!
//! 1. A `policy` stop surfaces at once; neither plan B nor a re-plan may route
//!    around it.
//! 2. Order 3, `improve`, when the fingerprint has recurred to the promotion
//!    threshold or the error is `unknown`, once per fingerprint and within
//!    the item's improvement budget. It files an `internal` item and does not
//!    stop the ladder: the next call returns the rung it would otherwise
//!    have chosen.
//! 3. A need only the user can meet counts as order 4 (6.4): plan B if the
//!    item has one, else the item parks in its typed blocked state and the
//!    parent asks. It is never re-planned around.
//! 4. The item's own rungs, skipped by error class: order 0 retries transient
//!    subclasses only; order 1 repairs, then switches worker, and a failed
//!    verification switches after one repair, since a worker that claimed
//!    more than it did once is escalated rather than asked again; a capability
//!    gap, or a missing dependency (an install), skips both and goes to
//!    order 2 (2a acquire, 2b build when the host confirms the tool does not
//!    exist, 2c request when the gap is capacity), one per gap; a spent
//!    budget skips all three.
//! 5. Inside a graph, plan B, then one re-plan by the parent.
//! 6. Surface, carrying the ladder.
//!
//! Budgets are per rung and per item ([`RungBudgets`]); a rung whose budget
//! is spent is skipped, never retried.

mod summary;
#[cfg(test)]
mod tests;

pub use summary::summary;

use chrono::{DateTime, Utc};
use rustykrab_core::work::{
    BlockedReason, ErrorClass, EventKind, Rung, RungBudgets, RungEvent, Trigger, WorkError,
    WorkEvent,
};
use serde::{Deserialize, Serialize};

use crate::errors::{gap_of, is_defect, unnamed_gap, Gap, GapKind, DEFAULT_PROMOTE_THRESHOLD};

/// Every rung, in the order the ladder climbs them.
pub const RUNGS: [Rung; 10] = [
    Rung::Retry,
    Rung::Repair,
    Rung::SwitchWorker,
    Rung::Acquire,
    Rung::Build,
    Rung::Request,
    Rung::Improve,
    Rung::PlanB,
    Rung::Replan,
    Rung::Surface,
];

/// How a rung is stored: a `rung` event whose `reason` is the
/// [`RungEvent`] as JSON, so the ladder, its errors and their fingerprints
/// re-derive from the store after a restart.
pub fn encode_rung_event(event: &RungEvent) -> String {
    serde_json::to_string(event).unwrap_or_default()
}

/// The [`RungEvent`] a stored `rung` event carries, or `None` for any other
/// event or an unreadable reason.
pub fn rung_event(event: &WorkEvent) -> Option<RungEvent> {
    if event.kind != EventKind::Rung {
        return None;
    }
    serde_json::from_str(event.reason.as_deref()?).ok()
}

/// An item's ladder as its events record it, oldest rung first.
pub fn ladder_of_events(events: &[WorkEvent]) -> Vec<RungEvent> {
    events.iter().filter_map(rung_event).collect()
}

/// The error behind the latest rung that carried one (section 9's
/// `last_error`).
pub fn last_error(events: &[WorkEvent]) -> Option<WorkError> {
    events
        .iter()
        .rev()
        .filter_map(rung_event)
        .find_map(|r| r.error)
}

/// A rung's place in [`RUNGS`], for "the highest order reached".
fn rank(rung: Rung) -> u8 {
    match rung {
        Rung::Retry => 0,
        Rung::Repair => 1,
        Rung::SwitchWorker => 2,
        Rung::Acquire => 3,
        Rung::Build => 4,
        Rung::Request => 5,
        Rung::Improve => 6,
        Rung::PlanB => 7,
        Rung::Replan => 8,
        Rung::Surface => 9,
    }
}

/// An item's ladder: the rungs climbed so far and what each rung may spend.
/// Every counter is derived from `history`, so the state is exactly what the
/// store's rung events say.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LadderState {
    pub history: Vec<RungEvent>,
    pub budgets: RungBudgets,
}

impl LadderState {
    pub fn new(budgets: RungBudgets) -> Self {
        LadderState {
            history: Vec::new(),
            budgets,
        }
    }

    /// How many times `rung` has been climbed on this item.
    pub fn used(&self, rung: Rung) -> u32 {
        let n = self.history.iter().filter(|e| e.rung == rung).count();
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    /// The item's budget for `rung`. `None` for plan B and surfacing, which
    /// the graph and policy bound rather than a count.
    pub fn budget(&self, rung: Rung) -> Option<u32> {
        let b = &self.budgets;
        match rung {
            Rung::Retry => Some(b.retries),
            Rung::Repair => Some(b.repairs),
            Rung::SwitchWorker => Some(b.worker_switches),
            Rung::Acquire => Some(b.acquisitions),
            Rung::Build => Some(b.builds),
            Rung::Request => Some(b.requests),
            Rung::Improve => Some(b.improvements),
            Rung::Replan => Some(b.replans),
            Rung::PlanB | Rung::Surface => None,
        }
    }

    /// What is left of `rung`'s budget; unbounded rungs report `u32::MAX`.
    /// On a parent, `left(Rung::Replan)` is the `replans_left_on_parent` its
    /// children's ladders take.
    pub fn left(&self, rung: Rung) -> u32 {
        self.budget(rung)
            .map_or(u32::MAX, |b| b.saturating_sub(self.used(rung)))
    }

    /// The highest rung climbed, in ladder order.
    pub fn highest(&self) -> Option<Rung> {
        self.history.iter().map(|e| e.rung).max_by_key(|r| rank(*r))
    }

    /// The order label of [`Self::highest`], or `none` when the ladder was
    /// not climbed.
    pub fn order_reached(&self) -> &'static str {
        self.highest().map_or("none", |r| r.order())
    }
}

/// What the controller knows about a failure when it asks for the next move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LadderContext<'a> {
    /// The classified failure.
    pub error: &'a WorkError,
    /// How many times its fingerprint has been seen ([`crate::errors::Recurrence::count`]).
    pub recurrence_count: u32,
    /// The count at which recurrence promotes to order 3
    /// ([`crate::errors::Recurrence::promote_threshold`]).
    pub promote_threshold: u32,
    /// The item has a `conditional_on_failure` downstream not yet released.
    pub has_plan_b: bool,
    /// Re-plans the item's parent may still run; 0 for an item with no
    /// parent.
    pub replans_left_on_parent: u32,
    /// A policy check (scope, single writer, a ceiling) stopped the work.
    pub policy_stop: bool,
    /// Only the user can meet the need: a credential only they hold, a
    /// consent, a decision outside delegated judgment.
    pub only_user_can_meet: bool,
    /// For a tool gap, whether the host confirms the tool exists (load it) or
    /// does not (build it). `None` when unchecked, which acquires first.
    pub tool_exists: Option<bool>,
}

impl<'a> LadderContext<'a> {
    /// A context for an item with no parent, no plan B and no recurrence.
    pub fn new(error: &'a WorkError) -> Self {
        LadderContext {
            error,
            recurrence_count: 0,
            promote_threshold: DEFAULT_PROMOTE_THRESHOLD,
            has_plan_b: false,
            replans_left_on_parent: 0,
            policy_stop: false,
            only_user_can_meet: false,
            tool_exists: None,
        }
    }
}

/// A capability the ladder's order 2 files a `capability` item for. The
/// original item gains a `blocks` edge on it and parks until it is `done`
/// (section 8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityNeed {
    pub gap: GapKind,
    /// The tool, credential, resource or topic missing.
    pub subject: String,
    /// When the capability item may become ready: on the credential for a
    /// credential, on the answer for a consent, at once otherwise.
    pub trigger: Trigger,
}

/// Why the ladder surfaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceReason {
    /// A policy stop, which surfaces at once.
    PolicyStop,
    /// Every rung is spent: budgets, plan B and the parent's re-plan.
    LadderSpent,
}

/// What a surfaced message carries (sections 6.6 and 8: "surfacing carries
/// the ladder"), so the user answers once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Surfacing {
    pub reason: SurfaceReason,
    /// The failure that ended the climb.
    pub error: WorkError,
    /// Every rung climbed, with its error and outcome.
    pub rungs: Vec<RungEvent>,
    /// The highest rung climbed; `None` when the ladder was not climbed.
    pub reached: Option<Rung>,
    /// What is being asked of the user.
    pub ask: String,
}

impl Surfacing {
    /// The order label reached, as the parent's message names it.
    pub fn order_reached(&self) -> &'static str {
        self.reached.map_or("none", |r| r.order())
    }

    /// The "what was tried at each order" paragraph.
    pub fn summary(&self) -> String {
        summary::summarise(&self.rungs)
    }
}

/// The ladder's next move.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Order 0: same worker, same brief.
    Retry,
    /// Order 1: re-run with the failure evidence and the error class; with a
    /// diagnosis step when the classifier could not name the failure or the
    /// same failure came back after a repair.
    Repair { with_diagnosis: bool },
    /// Order 1: a different worker kind or model.
    SwitchWorker,
    /// Order 2a: load the tool, or file a `capability` item with the need's
    /// trigger and park the work.
    Acquire(CapabilityNeed),
    /// Order 2b: file a `capability` build item (a tool, skill, MCP adapter
    /// or worker adapter) and resume once it is done.
    Build(CapabilityNeed),
    /// Order 2c: file a capacity request routed by policy.
    Request(CapabilityNeed),
    /// Order 3: file the `internal` item for this fingerprint
    /// ([`crate::errors::internal_item_draft`]); the ladder then continues.
    Improve { fingerprint: String },
    /// Fail the item and release its `conditional_on_failure` plan B.
    PlanB,
    /// Fail the item and let its parent run its re-plan.
    Replan,
    /// Order 4: tell the user.
    Surface(Surfacing),
    /// A need only the user can meet and no plan B: park the item in this
    /// blocked state and let the parent surface the question. Nothing
    /// cascades from a question not yet answered.
    Park(BlockedReason),
}

impl Decision {
    /// The rung to [`record`] for this move. Parking counts as order 4.
    pub fn rung(&self) -> Rung {
        match self {
            Decision::Retry => Rung::Retry,
            Decision::Repair { .. } => Rung::Repair,
            Decision::SwitchWorker => Rung::SwitchWorker,
            Decision::Acquire(_) => Rung::Acquire,
            Decision::Build(_) => Rung::Build,
            Decision::Request(_) => Rung::Request,
            Decision::Improve { .. } => Rung::Improve,
            Decision::PlanB => Rung::PlanB,
            Decision::Replan => Rung::Replan,
            Decision::Surface(_) | Decision::Park(_) => Rung::Surface,
        }
    }
}

/// The next move for a failed item.
pub fn next(state: &LadderState, ctx: &LadderContext) -> Decision {
    let err = ctx.error;
    if ctx.policy_stop || err.class == ErrorClass::Policy {
        return Decision::Surface(surfacing(state, err, SurfaceReason::PolicyStop));
    }
    if improve_due(state, ctx) {
        return Decision::Improve {
            fingerprint: err.fingerprint.clone(),
        };
    }
    let plan_b = ctx.has_plan_b && state.used(Rung::PlanB) == 0;
    if ctx.only_user_can_meet {
        return if plan_b {
            Decision::PlanB
        } else {
            Decision::Park(park_reason(err))
        };
    }
    if let Some(decision) = own_rung(state, ctx) {
        return decision;
    }
    if plan_b {
        return Decision::PlanB;
    }
    if ctx.replans_left_on_parent > 0 {
        return Decision::Replan;
    }
    Decision::Surface(surfacing(state, err, SurfaceReason::LadderSpent))
}

/// Write a climbed rung back into the state.
pub fn record(
    state: &mut LadderState,
    rung: Rung,
    error: WorkError,
    outcome: &str,
    at: DateTime<Utc>,
) {
    state.history.push(RungEvent {
        rung,
        at,
        error: Some(error),
        outcome: outcome.to_string(),
    });
}

fn same_fingerprint(event: &RungEvent, err: &WorkError) -> bool {
    event
        .error
        .as_ref()
        .is_some_and(|e| e.fingerprint == err.fingerprint)
}

/// Order 3 is due when the failure recurs, could not be classified, or is a
/// capability gap that names nothing, and has not already been filed for
/// this fingerprint on this item.
fn improve_due(state: &LadderState, ctx: &LadderContext) -> bool {
    let err = ctx.error;
    let promoted = is_defect(err)
        || unnamed_gap(err).is_some()
        || ctx.recurrence_count >= ctx.promote_threshold.max(1);
    promoted
        && state.left(Rung::Improve) > 0
        && !state
            .history
            .iter()
            .any(|e| e.rung == Rung::Improve && same_fingerprint(e, err))
}

/// Orders 0 to 2, as the error class allows.
fn own_rung(state: &LadderState, ctx: &LadderContext) -> Option<Decision> {
    let err = ctx.error;
    if let Some(gap) = gap_of(err) {
        return capability(state, ctx, gap);
    }
    if err.class == ErrorClass::Budget {
        // The item's own budget is spent; running it again would overrun it.
        return None;
    }
    if err.subclass.is_transient() && state.left(Rung::Retry) > 0 {
        return Some(Decision::Retry);
    }
    if err.class == ErrorClass::Verification && state.left(Rung::SwitchWorker) > 0 {
        let repaired = state.history.iter().any(|e| {
            e.rung == Rung::Repair
                && e.error
                    .as_ref()
                    .is_some_and(|x| x.class == ErrorClass::Verification)
        });
        if repaired {
            return Some(Decision::SwitchWorker);
        }
    }
    if state.left(Rung::Repair) > 0 {
        let repaired_before = state
            .history
            .iter()
            .any(|e| e.rung == Rung::Repair && same_fingerprint(e, err));
        return Some(Decision::Repair {
            with_diagnosis: is_defect(err) || repaired_before,
        });
    }
    if state.left(Rung::SwitchWorker) > 0 {
        return Some(Decision::SwitchWorker);
    }
    None
}

/// Order 2: one rung per gap, chosen by what is missing. A gap that names
/// nothing files no capability item: order 3 has triaged it, and the climb
/// goes on to plan B, the parent's re-plan or the user.
fn capability(state: &LadderState, ctx: &LadderContext, gap: Gap) -> Option<Decision> {
    if gap.subject.trim().is_empty() {
        return None;
    }
    let rung = match gap.kind {
        GapKind::Tool if ctx.tool_exists == Some(false) => Rung::Build,
        GapKind::Capacity => Rung::Request,
        _ => Rung::Acquire,
    };
    let filed_for_this_gap = state.history.iter().any(|e| {
        e.rung == rung
            && e.error
                .as_ref()
                .and_then(gap_of)
                .is_some_and(|g| g.same_need(&gap))
    });
    if filed_for_this_gap || state.left(rung) == 0 {
        return None;
    }
    let trigger = match gap.kind {
        GapKind::Credential => Trigger::OnCredential(gap.subject.clone()),
        GapKind::Consent => Trigger::OnAnswer(gap.subject.clone()),
        _ => Trigger::Now,
    };
    let need = CapabilityNeed {
        gap: gap.kind,
        subject: gap.subject,
        trigger,
    };
    Some(match rung {
        Rung::Build => Decision::Build(need),
        Rung::Request => Decision::Request(need),
        _ => Decision::Acquire(need),
    })
}

/// The typed blocked state for a need only the user can meet (section 7).
fn park_reason(err: &WorkError) -> BlockedReason {
    match gap_of(err).map(|g| g.kind) {
        Some(GapKind::Credential) => BlockedReason::NeedsCredential,
        Some(GapKind::Consent) => BlockedReason::NeedsConsent,
        Some(GapKind::Tool | GapKind::Install) => BlockedReason::NeedsTool,
        _ => BlockedReason::NeedsDecision,
    }
}

fn surfacing(state: &LadderState, err: &WorkError, reason: SurfaceReason) -> Surfacing {
    Surfacing {
        reason,
        error: err.clone(),
        rungs: state.history.clone(),
        reached: state.highest(),
        ask: summary::ask(reason, err),
    }
}
