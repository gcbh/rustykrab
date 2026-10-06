//! Reading the store back: the snapshot every decision computes over, an
//! item's ladder history, and what its events say about its next run.
//!
//! The snapshot is every live row (closed ones included, so a parent whose
//! children have all closed still reads as a parent and a closed upstream
//! still satisfies its edges) and every edge with an open end. Aging keeps
//! the live table bounded (4.6).

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use rustykrab_core::work::{
    BlockedReason, EventKind, Rung, RungEvent, Status, Trigger, WorkEvent, WorkItem, WorkItemId,
};
use rustykrab_core::Error;
use rustykrab_store::WorkFilter;

use crate::errors::{gap_of, GapKind, LearnedRule, Recurrence, CLASSIFIER_RULE};
use crate::graph::Snapshot;
use crate::ladder::LadderState;

use super::Controller;

/// Who verified a landed classifier rule: the controller, by replaying the
/// failure its `internal` item was filed for.
pub(super) const REPLAYED: &str = "replay";

/// The outcome a taken worker switch records: `switched from <worker>`.
pub(super) const SWITCHED: &str = "switched from ";

/// What a switch after a failed verification adds to its outcome:
/// `switched from <worker>; escalating above tier <n>`. Later leases skip
/// every worker at or below that tier (sections 5 and 8).
pub(super) const ESCALATING_ABOVE: &str = "; escalating above tier ";

/// The reason an approval writes on each item it releases: the audit
/// record of the answer. The approval also clears `held_by`; the marker is
/// still read so a hold an older build released (leaving `held_by` set)
/// reads as released.
pub(super) fn approval_marker(question: &str) -> String {
    format!("approved {question}")
}

/// A rung event's `reason` is its [`RungEvent`] as JSON
/// ([`crate::ladder::encode_rung_event`]), so the ladder is re-derived from
/// the store after a restart, errors and fingerprints included.
pub(super) fn encode_rung(event: &RungEvent) -> String {
    crate::ladder::encode_rung_event(event)
}

pub(super) fn decode_rung(event: &WorkEvent) -> Option<RungEvent> {
    crate::ladder::rung_event(event)
}

/// An item's ladder from its events.
pub(super) fn ladder_from(events: &[WorkEvent], item: &WorkItem) -> LadderState {
    LadderState {
        history: events.iter().filter_map(decode_rung).collect(),
        budgets: item.budget.rungs,
    }
}

/// What an item's events say about its next run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct History {
    /// Workers a taken worker switch moved the item away from.
    pub excluded: HashSet<String>,
    /// Set when the next run is a repair (anything but a plain retry after
    /// a failure, or a resume after a lost lease): the last error or the
    /// resume note, for the brief's `last_error`.
    pub repair: Option<String>,
    /// Tools a capability rung acquired or built, to activate up front.
    pub activate: Vec<String>,
    /// Set once a failed verification escalated the item: leases go only
    /// to workers above this cost tier.
    pub floor: Option<u32>,
}

pub(super) fn history(events: &[WorkEvent]) -> History {
    let mut out = History::default();
    let mut last_worker: Option<String> = None;
    for event in events {
        match event.kind {
            EventKind::Lease => {
                last_worker = Some(
                    event
                        .actor
                        .strip_prefix("worker:")
                        .unwrap_or(&event.actor)
                        .to_string(),
                );
            }
            EventKind::Rung => {
                let Some(rung) = decode_rung(event) else {
                    continue;
                };
                if rung.rung == Rung::SwitchWorker && rung.outcome.starts_with(SWITCHED) {
                    if let Some(w) = &last_worker {
                        out.excluded.insert(w.clone());
                    }
                    let tier = rung
                        .outcome
                        .split_once(ESCALATING_ABOVE)
                        .and_then(|(_, n)| n.trim().parse::<u32>().ok());
                    if let Some(tier) = tier {
                        out.floor = Some(out.floor.map_or(tier, |f| f.max(tier)));
                    }
                }
                if matches!(rung.rung, Rung::Acquire | Rung::Build) {
                    if let Some(gap) = rung.error.as_ref().and_then(gap_of) {
                        if gap.kind == GapKind::Tool && !out.activate.contains(&gap.subject) {
                            out.activate.push(gap.subject);
                        }
                    }
                }
                out.repair = match rung.rung {
                    Rung::Retry => None,
                    _ => Some(match &rung.error {
                        Some(e) => {
                            format!("{}/{}: {}", e.class.as_str(), e.subclass.as_str(), e.detail)
                        }
                        None => rung.outcome.clone(),
                    }),
                };
            }
            EventKind::Resume if event.to == Some(Status::Ready) => {
                out.repair = event.reason.clone();
            }
            _ => {}
        }
    }
    out
}

/// The error behind an item's latest rung, for a failed input's error class
/// and a notice's "not done" line.
pub(super) fn last_error(events: &[WorkEvent]) -> Option<rustykrab_core::work::WorkError> {
    crate::ladder::last_error(events)
}

impl Controller {
    /// Every live row and every edge with an open end, with released
    /// approval holds cleared and fired non-time triggers marked.
    pub(super) async fn load(&self) -> Result<Snapshot, Error> {
        self.ensure_spent().await?;
        let mut items = self
            .store
            .work_list(&WorkFilter {
                include_closed: true,
                ..WorkFilter::default()
            })
            .await?;
        let edges = self.store.work_edges_all_open().await?;
        for item in items.iter_mut() {
            let Some(question) = item.held_by.clone() else {
                continue;
            };
            let pending = item.status == Status::Blocked(BlockedReason::NeedsConsent);
            if pending || item.status.is_closed() {
                continue;
            }
            if self.approved(&item.id, &question).await? {
                item.held_by = None;
            }
        }
        let fired: Vec<WorkItemId> = items
            .iter()
            .filter(|i| self.trigger_fired(&i.trigger))
            .map(|i| i.id.clone())
            .collect();
        Ok(Snapshot::new(items, edges).with_fired(fired))
    }

    /// `on_mcp` and `on_credential` fire when the host has the server or
    /// the credential. `on_answer` waits for the question router (Phase 4).
    fn trigger_fired(&self, trigger: &Trigger) -> bool {
        match trigger {
            Trigger::OnMcp(server) => self.catalog.mcp_server_configured(server),
            Trigger::OnCredential(name) => self.catalog.credential_available(name),
            _ => false,
        }
    }

    /// Whether `item`'s hold on `question` was released by an approval.
    async fn approved(&self, item: &str, question: &str) -> Result<bool, Error> {
        if let Some(known) = self.state().approved.get(item).copied() {
            return Ok(known);
        }
        let marker = approval_marker(question);
        let found = self
            .store
            .work_events(item)
            .await?
            .iter()
            .any(|e| e.reason.as_deref() == Some(marker.as_str()));
        self.state().approved.insert(item.to_string(), found);
        Ok(found)
    }

    /// An item's ladder as its rung events record it.
    pub(super) async fn ladder_of(&self, item: &WorkItem) -> Result<LadderState, Error> {
        let events = self.store.work_events(&item.id).await?;
        Ok(ladder_from(&events, item))
    }

    /// Rebuild fingerprint recurrence from recent rung events: one count per
    /// item a fingerprint failed on.
    pub(super) async fn load_recurrence(&self, now: DateTime<Utc>) -> Result<(), Error> {
        let since = now
            .checked_sub_signed(self.config.recurrence_window)
            .unwrap_or(DateTime::<Utc>::MIN_UTC);
        let events = self.store.work_events_since(since).await?;
        let mut seen: HashSet<(String, String)> = HashSet::new();
        let mut recurrence = Recurrence::with_threshold(self.config.promote_threshold);
        for event in &events {
            let Some(error) = decode_rung(event).and_then(|r| r.error) else {
                continue;
            };
            if seen.insert((event.item.clone(), error.fingerprint.clone())) {
                recurrence.observe(&error.fingerprint);
            }
        }
        self.state().recurrence = recurrence;
        Ok(())
    }

    /// Rebuild the classifier rules `internal` items landed from their
    /// `classifier_rule` evidence: the rows the controller verified by
    /// replay, oldest first (section 9).
    pub(super) async fn load_learned(&self) -> Result<(), Error> {
        let rows = self.store.work_evidence_of_kind(CLASSIFIER_RULE).await?;
        let learned: Vec<LearnedRule> = rows
            .iter()
            .filter(|e| e.verified_by.as_deref() == Some(REPLAYED))
            .filter_map(|e| LearnedRule::parse(&e.item, &e.reference).ok())
            .collect();
        self.state().learned = learned;
        Ok(())
    }

    /// The conversation a run of `item` continues, when `item` is a
    /// scheduled job's firing (plan section 13: `scheduled_jobs` gains
    /// `work_item_id`): the job's persistent conversation, so the firing
    /// sees the job's earlier runs as a job run always has. The run's id is
    /// that conversation's, which keeps the item's `run` evidence pointing
    /// at the transcript. Read from the job row, which no model writes, so
    /// no filing can point a run at a conversation of its choosing.
    pub(super) async fn continued_conversation(&self, item: &str) -> Option<String> {
        match self.store.jobs().job_for_work_item(item).await {
            Ok(job) => job
                .and_then(|j| j.conversation_id)
                .filter(|c| uuid::Uuid::parse_str(c).is_ok()),
            Err(e) => {
                tracing::warn!(item = %item, error = %e, "scheduled firing not looked up");
                None
            }
        }
    }

    /// The worker holding `item`: its live run here, else its lease row.
    pub(super) async fn worker_of(&self, item: &str) -> Result<Option<String>, Error> {
        if let Some(run) = self.state().runs.get(item) {
            return Ok(Some(run.worker.clone()));
        }
        Ok(self.store.work_lease_get(item).await?.map(|l| l.worker))
    }
}
