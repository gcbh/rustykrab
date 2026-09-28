//! Plan 6.7 for runs a restart does not kill (Phase 5).
//!
//! On the first tick every active leaf this process is not running is an
//! orphan, and the sweep returns it to `ready` with a repair note: a local
//! run died with the process. A peer's run did not. It is a task on another
//! machine, which went on while this controller was down. So before any
//! orphan is returned, each one leased to a worker that is still registered
//! is re-checked against that worker: [`Worker::resumable`] asks whether the
//! run recorded as the item's `run` evidence is still the worker's (a peer
//! asks its node for the task). One that is keeps its lease: the controller
//! runs the same brief under the same run id again, which the worker takes
//! as a re-attach to the run it has, and records a `resume` event saying
//! so. Every other orphan returns to `ready` as before. Nothing fails
//! because a restart happened.
//!
//! [`Worker::resumable`]: crate::worker::Worker::resumable

use chrono::Utc;
use rustykrab_core::work::{EventKind, Status, WorkEvent, WorkItem, WorkItemId, WorkKind};
use rustykrab_core::Error;

use super::batch::Batch;
use super::brief::{brief_for, RUN};
use super::load::history;
use super::tick::spawn;
use super::Controller;

impl Controller {
    /// Re-attach every orphan whose worker still holds its run; returns
    /// the items re-attached, which are then this process's runs, so the
    /// sweep's return-to-ready passes them by. A store error on one item
    /// leaves that item to the sweep.
    pub(super) async fn reattach(&self, b: &Batch) -> Vec<WorkItemId> {
        let orphans: Vec<WorkItem> = {
            let state = self.state();
            b.snap
                .items()
                .iter()
                .filter(|i| {
                    matches!(i.status, Status::Leased | Status::Running)
                        && !b.snap.has_children(&i.id)
                        && !state.runs.contains_key(&i.id)
                        && !state.finished.contains_key(&i.id)
                })
                .cloned()
                .collect()
        };
        let mut out = Vec::new();
        for item in orphans {
            match self.reattach_one(&item).await {
                Ok(true) => out.push(item.id),
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(item = %item.id, error = %e, "re-attach check failed; the sweep returns it to ready");
                }
            }
        }
        out
    }

    async fn reattach_one(&self, item: &WorkItem) -> Result<bool, Error> {
        let Some(lease) = self.store.work_lease_get(&item.id).await? else {
            return Ok(false);
        };
        let Some(worker) = self.worker(&lease.worker) else {
            return Ok(false);
        };
        let evidence = self.store.work_evidence_list(&item.id).await?;
        let Some(run) = evidence
            .iter()
            .rev()
            .find(|e| e.kind == RUN)
            .map(|e| e.reference.clone())
        else {
            return Ok(false);
        };
        if !worker.resumable(&run).await {
            return Ok(false);
        }
        // The brief as the lease gave it: the inputs recorded on the lease,
        // the history as it stood. The worker has the run already; the brief
        // is what it would resubmit if its node had lost the task.
        let hist = history(&self.store.work_events(&item.id).await?);
        let prior = if hist.repair.is_some() {
            evidence
        } else {
            Vec::new()
        };
        let mut brief = brief_for(item, lease.inputs.clone(), Vec::new(), prior, &hist);
        brief.run = Some(run.clone());
        brief.capability = match item.kind {
            WorkKind::Capability => self.capability_mode(&item.id).await?,
            _ => None,
        };
        self.store.work_lease_heartbeat(&item.id).await?;
        self.store
            .work_event_append(&WorkEvent {
                item: item.id.clone(),
                at: Utc::now(),
                kind: EventKind::Resume,
                from: None,
                to: None,
                actor: "controller".to_string(),
                reason: Some(format!(
                    "the controller restarted; {} still holds run {run}, so the lease stands and \
                     the controller re-attached to it",
                    lease.worker
                )),
                upstream: None,
                origin: None,
                evidence_ref: None,
            })
            .await?;
        tracing::info!(item = %item.id, worker = %lease.worker, %run, "re-attached to a run that survived the restart");
        let run = spawn(worker, brief, self.clock.now());
        self.state().runs.insert(item.id.clone(), run);
        Ok(true)
    }
}
