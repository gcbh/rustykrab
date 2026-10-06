//! Phase 6 in the controller (plan sections 10 and 11): the review facets
//! written with a filing, a new proposal held for its review, and a
//! review decision taken on the review surface applied to its proposal.
//!
//! A proposal is not work a worker runs: it is a change a human reviews.
//! Filed, it waits in `blocked(needs_consent)` and is projected to an
//! issue. Accepted, a routing proposal (one that names its move as a
//! `routing_default` artifact) moves its class's default tier in the worker
//! registry's `routing_defaults` and closes `done`: the move is data, not a
//! change for a worker to make, and the only way a default ever moves
//! (section 10). Any other accepted proposal becomes a `code` item filed as
//! `FilingSource::Proposal` (the one path besides the delivery import that
//! may file code) with a `discovered_from` edge onto the proposal, and the
//! proposal closes `done`; declined, it closes `cancelled(requested)`; an
//! amendment is recorded and carried into the `code` item when it is
//! accepted. Each decision is one transaction with its `review` event.

use std::collections::HashSet;

use rustykrab_core::proposal::{ReviewDecision, ReviewOutcome};
use rustykrab_core::work::{
    ArtifactRef, BlockedReason, DraftEdge, EdgeKind, EventKind, ItemRef, Status, WorkFacets,
    WorkItem, WorkItemDraft, WorkKind, WorkPlan,
};
use rustykrab_core::Error;
use rustykrab_store::WorkOp;
use rustykrab_tools::work_backend::Provenance;

use crate::graph::{Accepted, FilingSource};

use super::batch::Batch;
use super::filing::describe_rejection;
use super::notice::{short, Cause};
use super::Controller;

/// Why a new proposal waits, on its transition into `blocked(needs_consent)`.
pub const AWAITING_REVIEW: &str = "awaiting review on the review surface";

/// The prefix of an amendment's `review` event, which an acceptance reads
/// back into the `code` item's constraints.
pub const AMENDED: &str = "amended: ";

impl Controller {
    /// Queue the review facets of a filing's new items with its rows, so
    /// an item never exists without them. `accepted.items` is in filing
    /// order, one per draft.
    pub(super) fn record_facets(&self, b: &mut Batch, plan: &WorkPlan, accepted: &Accepted) {
        if plan.items.len() != accepted.items.len() {
            return;
        }
        for (draft, item) in plan.items.iter().zip(&accepted.items) {
            if let Some(facets) = WorkFacets::of_draft(draft, item.kind) {
                b.ops.push(WorkOp::Facets {
                    item: item.id.clone(),
                    facets,
                });
            }
        }
    }

    /// A new proposal waits for its review in `blocked(needs_consent)`, so
    /// no worker ever leases it, and the user is told it was filed.
    pub(super) fn hold_for_review(&self, b: &mut Batch, accepted: &Accepted) {
        for item in &accepted.items {
            if item.kind != WorkKind::Proposal || b.status(&item.id) != Some(Status::Queued) {
                continue;
            }
            b.move_to(
                &item.id,
                Status::Blocked(BlockedReason::NeedsConsent),
                "controller",
                AWAITING_REVIEW,
            );
            b.notify(
                &item.id,
                Cause::Asked {
                    item: item.id.clone(),
                    text: "a proposal was filed; accept, decline or amend it on the review \
                           surface"
                        .to_string(),
                },
            );
        }
    }

    pub(super) async fn review_decision_locked(
        &self,
        proposal: &str,
        decision: ReviewDecision,
        actor: &str,
    ) -> Result<ReviewOutcome, Error> {
        let now = self.clock.now();
        let mut b = Batch::new(self.load().await?, now);
        let item = b
            .snap
            .item(proposal)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("work item {proposal}")))?;
        if item.kind != WorkKind::Proposal {
            return Err(Error::Internal(format!(
                "{proposal} is a {} item; only a proposal takes a review decision",
                item.kind.as_str()
            )));
        }
        let outcome =
            |status: Status, code_item: Option<String>, note: Option<String>| ReviewOutcome {
                proposal: proposal.to_string(),
                decision: decision.name().to_string(),
                status,
                code_item,
                note,
            };
        if item.status.is_closed() {
            return Ok(outcome(
                item.status,
                None,
                Some(format!("already decided: the proposal is {}", item.status)),
            ));
        }
        match &decision {
            ReviewDecision::Amend { text } => {
                b.note(
                    proposal,
                    EventKind::Review,
                    actor,
                    format!("{AMENDED}{}", text.trim()),
                );
                self.commit(b, &mut HashSet::new()).await?;
                Ok(outcome(item.status, None, None))
            }
            ReviewDecision::Decline { reason } => {
                let why = reason
                    .clone()
                    .filter(|r| !r.trim().is_empty())
                    .unwrap_or_else(|| "declined on the review surface".to_string());
                b.note(
                    proposal,
                    EventKind::Review,
                    actor,
                    format!("declined: {why}"),
                );
                let (changed, _) = b.cancel_tree(proposal, actor, Some(why));
                b.settle(changed);
                self.commit(b, &mut HashSet::new()).await?;
                Ok(outcome(
                    Status::Cancelled(rustykrab_core::work::CancelReason::Requested),
                    None,
                    None,
                ))
            }
            ReviewDecision::Accept if routing_move_of(&item).is_some() => {
                let (tier, class) = routing_move_of(&item).unwrap_or_default();
                self.store
                    .workers()
                    .set_default_tier(&class, tier, proposal, Some(item.title.trim()))
                    .await?;
                self.routing.defaults_moved().await;
                let moved = format!("the default tier for {class} moved to {tier}");
                b.note(
                    proposal,
                    EventKind::Review,
                    actor,
                    format!("accepted: {moved}"),
                );
                let changed = b.close(
                    proposal,
                    Status::Done,
                    actor,
                    format!("accepted on the review surface; {moved}"),
                );
                b.settle(changed);
                self.commit(b, &mut HashSet::new()).await?;
                Ok(outcome(Status::Done, None, Some(moved)))
            }
            ReviewDecision::Accept => {
                let amendments = self.amendments_of(proposal).await?;
                let plan = code_plan(&item, &amendments);
                let provenance = Provenance {
                    conversation_id: item.origin_conversation_id.clone(),
                    filed_by_item: None,
                    actor: actor.to_string(),
                };
                let accepted = self
                    .file_into(&mut b, &plan, &provenance, FilingSource::Proposal)
                    .map_err(|r| {
                        Error::Internal(format!(
                            "the accepted proposal's code item was rejected: {}",
                            describe_rejection(&r)
                        ))
                    })?;
                let code = accepted
                    .ids
                    .get(CODE_TMP)
                    .cloned()
                    .unwrap_or_else(|| accepted.root.clone());
                b.note(
                    proposal,
                    EventKind::Review,
                    actor,
                    format!("accepted: carried out as {}", short(&code)),
                );
                let mut changed = accepted.changed();
                changed.extend(b.close(
                    proposal,
                    Status::Done,
                    actor,
                    format!(
                        "accepted on the review surface; carried out as {} under \
                         verification and probation",
                        short(&code)
                    ),
                ));
                b.settle(changed);
                self.commit(b, &mut HashSet::new()).await?;
                Ok(outcome(Status::Done, Some(code), None))
            }
        }
    }

    /// The amendments recorded on a proposal, oldest first.
    async fn amendments_of(&self, proposal: &str) -> Result<Vec<String>, Error> {
        Ok(self
            .store
            .work_events(proposal)
            .await?
            .into_iter()
            .filter(|e| e.kind == EventKind::Review)
            .filter_map(|e| {
                e.reason
                    .as_deref()
                    .and_then(|r| r.strip_prefix(AMENDED))
                    .map(str::to_string)
            })
            .collect())
    }
}

const CODE_TMP: &str = "accepted";

/// The routing move a proposal names, if it is a routing proposal: its
/// `routing_default` artifact (`<tier> <class>`).
fn routing_move_of(proposal: &WorkItem) -> Option<(u32, String)> {
    proposal
        .artifact_refs
        .iter()
        .filter(|r| r.kind == rustykrab_core::proposal::ROUTING_DEFAULT)
        .find_map(|r| rustykrab_core::proposal::routing_move(&r.value))
}

/// The `code` item an accepted proposal becomes (section 10, Execution):
/// the proposal's objective as the change to make, its constraints and
/// amendments, its evidence and a pointer to the proposal, and a
/// `discovered_from` edge onto it. Its own root: the proposal is closed
/// by the same transaction and cannot be a parent.
fn code_plan(proposal: &WorkItem, amendments: &[String]) -> WorkPlan {
    let mut constraints = proposal.constraints.clone();
    constraints.push(
        "Run under verification: the change counts once the verifier accepts its evidence, \
         and the proposal's metric is then watched through probation (section 10)."
            .to_string(),
    );
    for a in amendments {
        constraints.push(format!("Amended on review: {a}"));
    }
    let mut artifact_refs = proposal.artifact_refs.clone();
    artifact_refs.push(ArtifactRef {
        kind: "item".to_string(),
        value: proposal.id.clone(),
    });
    let draft = WorkItemDraft {
        tmp: Some(CODE_TMP.to_string()),
        kind: Some(WorkKind::Code),
        title: format!("Carry out proposal: {}", proposal.title.trim()),
        objective: format!(
            "Carry out the accepted proposal {}. {}",
            short(&proposal.id),
            proposal.objective.trim()
        ),
        done_when: proposal.done_when.clone(),
        constraints,
        decisions_made: proposal.decisions_made.clone(),
        worker_kind: proposal.worker_kind,
        writable_resources: proposal.writable_resources.clone(),
        artifact_refs,
        edges: vec![DraftEdge {
            kind: EdgeKind::DiscoveredFrom,
            depends_on: ItemRef::Id(proposal.id.clone()),
        }],
        ..WorkItemDraft::default()
    };
    WorkPlan {
        root: ItemRef::Tmp {
            tmp: CODE_TMP.to_string(),
        },
        items: vec![draft],
        edges: Vec::new(),
        rationale: format!("accepted proposal {}", short(&proposal.id)),
    }
}
