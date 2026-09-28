//! What the evaluation reads, and the facts it derives from it once.
//!
//! Everything here is read from stored events, never from prose: a rung
//! event's reason is its `RungEvent` as JSON (the controller's encoding),
//! a transition carries its typed statuses, a lease event names its worker.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use rustykrab_core::work::{
    ArtifactRef, Edge, EdgeKind, ErrorClass, EventKind, RejectionReason, Rung, RungEvent, Status,
    WorkError, WorkEvent, WorkFacets, WorkItemId, WorkKind, WorkerKind,
};

/// One item, live or archived, as the evaluation needs it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemRecord {
    pub id: WorkItemId,
    pub kind: WorkKind,
    pub status: Status,
    pub title: String,
    pub parent: Option<WorkItemId>,
    pub created_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub artifact_refs: Vec<ArtifactRef>,
    /// Aged into `work_item_archive`: its row is a summary line.
    #[serde(default)]
    pub archived: bool,
}

/// Everything the pass reads about work in its window.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WorkRecords {
    pub items: Vec<ItemRecord>,
    /// Every event at or after the window's start, oldest first.
    pub events: Vec<WorkEvent>,
    /// The edges the items hold.
    pub edges: Vec<Edge>,
    pub facets: HashMap<WorkItemId, WorkFacets>,
}

/// A question the router delivered (Phase 4's `questions`), as the
/// avoidable-escalation criterion reads it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SurfacedQuestion {
    pub id: String,
    pub item: WorkItemId,
    pub class: String,
    #[serde(default)]
    pub options: Vec<String>,
    /// The answer standing judgment or the item recorded as the default.
    #[serde(default)]
    pub recorded_default: Option<String>,
    /// The channel it reached the user through; `None` when policy
    /// answered it without asking.
    #[serde(default)]
    pub delivered_via: Option<String>,
    #[serde(default)]
    pub answer: Option<String>,
    #[serde(default)]
    pub answered_at: Option<DateTime<Utc>>,
}

impl SurfacedQuestion {
    /// Surfaced to the user and answered with the recorded default: a
    /// lower rung could have produced the answer (section 10).
    pub fn is_avoidable(&self) -> bool {
        let norm = |s: &str| s.trim().to_ascii_lowercase();
        self.delivered_via.is_some()
            && matches!(
                (&self.answer, &self.recorded_default),
                (Some(a), Some(d)) if !d.trim().is_empty() && norm(a) == norm(d)
            )
    }
}

/// One worker's record for one class of work (Phase 3's `routing_record`
/// on `workers`, plan section 5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutingEntry {
    pub worker: String,
    pub worker_kind: WorkerKind,
    /// Lower is cheaper.
    pub cost_tier: u32,
    /// The class of work, e.g. `code:small`.
    pub class: String,
    pub verified_done: u32,
    pub claimed_not_verified: u32,
    pub escaped_defects: u32,
    #[serde(default)]
    pub review_rejections: u32,
    #[serde(default)]
    pub repairs: u32,
    #[serde(default)]
    pub cost: f64,
    #[serde(default)]
    pub probation: bool,
    /// Whether this worker is the class's default tier today.
    #[serde(default)]
    pub default_for_class: bool,
    /// The cost tier the class's default sits at today, when it is known.
    #[serde(default)]
    pub default_tier: Option<u32>,
    /// The latest items behind the record, oldest first: what a proposal
    /// citing the record points the reviewer at.
    #[serde(default)]
    pub items: Vec<WorkItemId>,
}

impl RoutingEntry {
    pub fn claims(&self) -> u32 {
        self.verified_done + self.claimed_not_verified
    }

    pub fn verified_rate(&self) -> Option<f64> {
        let claims = self.claims();
        (claims > 0).then(|| f64::from(self.verified_done) / f64::from(claims))
    }
}

/// One classified failure on one item.
#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    pub item: WorkItemId,
    pub error: WorkError,
    pub at: DateTime<Utc>,
}

/// What the criteria and metrics share, derived once from the records.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub items: BTreeMap<WorkItemId, ItemRecord>,
    /// Every rung climbed, per item, in order.
    pub rungs: BTreeMap<WorkItemId, Vec<(RungEvent, DateTime<Utc>)>>,
    /// Distinct `(item, fingerprint)` failures, first seen.
    pub failures: Vec<Failure>,
    /// Items the controller surfaced to the user, with when (a `surface`
    /// rung, or a park in a reason only the user can meet). Proposals are
    /// excluded: waiting for review is their design, not an escalation.
    pub escalated: BTreeMap<WorkItemId, DateTime<Utc>>,
    /// Items a user acted on (an event whose actor is `user...`).
    pub user_touched: BTreeSet<WorkItemId>,
    /// The workers that held each item's leases, in order.
    pub workers: BTreeMap<WorkItemId, Vec<String>>,
    /// When each item was first leased.
    pub leased_at: BTreeMap<WorkItemId, DateTime<Utc>>,
    /// Items that entered `verifying`: a result the worker claimed done.
    pub claimed: BTreeSet<WorkItemId>,
    /// Filing rejections, by reason, with the item they were recorded on.
    pub rejections: Vec<(String, WorkItemId, DateTime<Utc>)>,
    /// `sequential_split` warnings.
    pub split_warnings: Vec<(WorkItemId, DateTime<Utc>)>,
    /// Edges by the upstream they name.
    pub dependents: BTreeMap<WorkItemId, Vec<Edge>>,
    pub facets: HashMap<WorkItemId, WorkFacets>,
}

/// A rung event's `RungEvent`, as the controller encodes it.
pub fn rung_of(event: &WorkEvent) -> Option<RungEvent> {
    if event.kind != EventKind::Rung {
        return None;
    }
    serde_json::from_str(event.reason.as_deref()?).ok()
}

impl Facts {
    pub fn derive(records: &WorkRecords) -> Facts {
        let mut f = Facts {
            facets: records.facets.clone(),
            ..Facts::default()
        };
        for item in &records.items {
            f.items.insert(item.id.clone(), item.clone());
        }
        for edge in &records.edges {
            f.dependents
                .entry(edge.depends_on.clone())
                .or_default()
                .push(edge.clone());
        }
        let mut seen: BTreeSet<(WorkItemId, String)> = BTreeSet::new();
        for event in &records.events {
            let is_proposal = f
                .items
                .get(&event.item)
                .is_some_and(|i| i.kind == WorkKind::Proposal);
            if event.actor.starts_with("user") {
                f.user_touched.insert(event.item.clone());
            }
            match event.kind {
                EventKind::Rung => {
                    let Some(rung) = rung_of(event) else {
                        continue;
                    };
                    if let Some(error) = &rung.error {
                        if seen.insert((event.item.clone(), error.fingerprint.clone())) {
                            f.failures.push(Failure {
                                item: event.item.clone(),
                                error: error.clone(),
                                at: event.at,
                            });
                        }
                    }
                    if rung.rung == Rung::Surface && !is_proposal {
                        f.escalated.entry(event.item.clone()).or_insert(event.at);
                    }
                    f.rungs
                        .entry(event.item.clone())
                        .or_default()
                        .push((rung, event.at));
                }
                EventKind::Lease => {
                    let worker = event
                        .actor
                        .strip_prefix("worker:")
                        .unwrap_or(&event.actor)
                        .to_string();
                    f.workers
                        .entry(event.item.clone())
                        .or_default()
                        .push(worker);
                    f.leased_at.entry(event.item.clone()).or_insert(event.at);
                }
                EventKind::Transition | EventKind::Cascade | EventKind::Resume => {
                    match event.to {
                        Some(Status::Verifying) => {
                            f.claimed.insert(event.item.clone());
                        }
                        // A plan held for approval is a question too,
                        // but only one the controller asked: the
                        // cascade's holds are not escalations.
                        Some(Status::Blocked(r))
                            if r.needs_user()
                                && !is_proposal
                                && event.kind != EventKind::Cascade =>
                        {
                            f.escalated.entry(event.item.clone()).or_insert(event.at);
                        }
                        _ => {}
                    }
                }
                EventKind::Rejection => {
                    // The controller writes `<reason>: <detail>` per failed
                    // check, joined by `; `; only a word the rejection
                    // vocabulary knows counts, so a detail's own colons
                    // and semicolons cannot invent a reason.
                    let raw = event.reason.as_deref().unwrap_or_default();
                    let raw = raw
                        .strip_prefix("discovered drafts rejected: ")
                        .unwrap_or(raw);
                    for part in raw.split("; ") {
                        let word = part.split(':').next().unwrap_or_default().trim();
                        let known = serde_json::from_value::<RejectionReason>(
                            serde_json::Value::String(word.to_string()),
                        )
                        .is_ok();
                        if known {
                            f.rejections
                                .push((word.to_string(), event.item.clone(), event.at));
                        }
                    }
                }
                EventKind::Warning
                    if event
                        .reason
                        .as_deref()
                        .is_some_and(|r| r.contains("sequential_split")) =>
                {
                    f.split_warnings.push((event.item.clone(), event.at));
                }
                _ => {}
            }
        }
        f
    }

    pub fn item(&self, id: &str) -> Option<&ItemRecord> {
        self.items.get(id)
    }

    pub fn kind_of(&self, id: &str) -> Option<WorkKind> {
        self.items.get(id).map(|i| i.kind)
    }

    /// Whether an item has children among the records.
    pub fn is_parent(&self, id: &str) -> bool {
        self.items.values().any(|i| i.parent.as_deref() == Some(id))
    }

    /// Failures of one class.
    pub fn failures_of(&self, class: ErrorClass) -> impl Iterator<Item = &Failure> {
        self.failures.iter().filter(move |f| f.error.class == class)
    }

    /// Items holding an edge of `kind` onto `upstream`.
    pub fn dependents_of(&self, upstream: &str, kind: EdgeKind) -> Vec<&Edge> {
        self.dependents
            .get(upstream)
            .map(|edges| edges.iter().filter(|e| e.kind == kind).collect())
            .unwrap_or_default()
    }

    /// The worker that last held the item.
    pub fn worker_of(&self, id: &str) -> Option<&str> {
        self.workers
            .get(id)
            .and_then(|w| w.last())
            .map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustykrab_core::work::BlockedReason;

    fn event(item: &str, kind: EventKind, to: Option<Status>, reason: &str) -> WorkEvent {
        WorkEvent {
            item: item.into(),
            at: Utc::now(),
            kind,
            from: None,
            to,
            actor: "controller".into(),
            reason: Some(reason.into()),
            upstream: None,
            origin: None,
            evidence_ref: None,
        }
    }

    fn record(id: &str, kind: WorkKind) -> ItemRecord {
        ItemRecord {
            id: id.into(),
            kind,
            status: Status::Queued,
            title: String::new(),
            parent: None,
            created_at: Utc::now(),
            closed_at: None,
            artifact_refs: vec![],
            archived: false,
        }
    }

    #[test]
    fn rejections_count_only_known_reasons_and_proposals_never_escalate() {
        let needs = Some(Status::Blocked(BlockedReason::NeedsConsent));
        let records = WorkRecords {
            items: vec![
                record("p", WorkKind::Proposal),
                record("e", WorkKind::Personal),
            ],
            events: vec![
                event(
                    "e",
                    EventKind::Rejection,
                    None,
                    "too_many_items: 13 items: over 12; cycle: a -> b; nonsense: x",
                ),
                event(
                    "e",
                    EventKind::Rejection,
                    None,
                    "discovered drafts rejected: out_of_scope: the parent",
                ),
                event("p", EventKind::Transition, needs, "awaiting review"),
                event("e", EventKind::Transition, needs, "held for approval"),
                event("c", EventKind::Cascade, needs, "cascade"),
            ],
            ..WorkRecords::default()
        };
        let f = Facts::derive(&records);
        let reasons: Vec<&str> = f.rejections.iter().map(|(r, _, _)| r.as_str()).collect();
        assert_eq!(reasons, vec!["too_many_items", "cycle", "out_of_scope"]);
        assert!(f.escalated.contains_key("e"));
        assert!(!f.escalated.contains_key("p"));
        assert!(!f.escalated.contains_key("c"));
    }
}
