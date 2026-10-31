//! Read-only foreground demand through the controller's existing gates/matcher.
//! Counts describe potential dispatches, not a new scheduling queue or grant.
use super::{load::history, Controller};
use crate::{
    graph,
    handle::{ControlHandle, DispatchDemand, LockState},
};
use rustykrab_core::{
    work::{Status, WorkItem, WorkKind},
    Error,
};
use std::collections::{BTreeMap, HashSet};

fn project_review(item: &WorkItem) -> bool {
    item.kind == WorkKind::Research
        && item
            .artifact_refs
            .iter()
            .any(|a| a.kind == "dream_review_job")
        && item
            .artifact_refs
            .iter()
            .any(|a| a.kind == rustykrab_core::dream_review::REVIEW_ONLY && a.value == "true")
}

impl Controller {
    /// Observe current demand without leasing, running, answering, granting or
    /// changing work. Public for inspection before the ControlHandle bridge is
    /// integrated. Missing/unhealthy loop state is unknown, never idle.
    pub async fn foreground_dispatchability(&self) -> Result<Option<DispatchDemand>, Error> {
        let Ok(_loop) = self.loop_lock.try_lock() else {
            return Ok(None);
        };
        let Some(status) = ControlHandle::loop_status(self) else {
            return Ok(None);
        };
        if status.lock != Some(LockState::Held)
            || status.draining
            || status.consecutive_failed_ticks != 0
            || status.last_tick.is_none()
        {
            return Ok(None);
        }
        let observed_at = self.clock.now();
        if status
            .last_tick
            .is_none_or(|at| at > observed_at || observed_at - at > chrono::TimeDelta::seconds(60))
        {
            return Ok(None);
        }
        let snap = self.load().await?;
        let (load, models) = self.load_by_worker();
        let isolated: std::collections::HashMap<_, _> = self
            .state()
            .runs
            .iter()
            .map(|(id, run)| (id.clone(), run.isolated_repo.clone()))
            .collect();
        let busy: Vec<_> = snap
            .items()
            .iter()
            .filter(|i| i.status.is_active() && !snap.has_children(&i.id))
            .map(|i| {
                (
                    i.writable_resources.clone(),
                    isolated.get(&i.id).cloned().flatten(),
                )
            })
            .collect();
        // A durable active lease without a matching in-memory run cannot look
        // like idle capacity before the controller reconciles it. Include both.
        let mut active: HashSet<_> = isolated
            .keys()
            .filter(|id| !snap.item(id).is_some_and(project_review))
            .cloned()
            .collect();
        active.extend(
            snap.items()
                .iter()
                .filter(|i| i.status.is_active() && !snap.has_children(&i.id) && !project_review(i))
                .map(|i| i.id.clone()),
        );
        let mut waiting: BTreeMap<String, usize> = BTreeMap::new();
        let mut dispatchable = 0;
        for item in snap
            .items()
            .iter()
            .filter(|i| !i.status.is_closed() && i.kind != WorkKind::Proposal && !project_review(i))
        {
            let reason = if snap.has_children(&item.id) {
                Some("parent".to_string())
            } else if let Status::Blocked(reason) = item.status {
                Some(format!("blocked/{}", reason.as_str()))
            } else if item.status.is_active() {
                None
            } else if !graph::is_ready(&snap, &item.id, observed_at) {
                Some(
                    if item.held_by.is_some()
                        || snap
                            .ancestors(&item.id)
                            .iter()
                            .any(|id| snap.item(id).is_some_and(|p| p.held_by.is_some()))
                    {
                        "owner_hold"
                    } else if item.expires_at.is_some_and(|at| at <= observed_at) {
                        "expiry"
                    } else if !snap.trigger_fired(&item.id, observed_at) {
                        "trigger"
                    } else if !graph::edge_summary(&snap, &item.id).satisfied {
                        "dependencies"
                    } else {
                        "ancestor_gate"
                    }
                    .to_string(),
                )
            } else if !item
                .preconditions
                .iter()
                .all(|p| self.catalog.precondition_holds(p))
            {
                Some("preconditions".to_string())
            } else {
                let events = self.store.work_events(&item.id).await?;
                let history = history(&events);
                if self.pick(item, &history, &load, &models, &busy).is_some() {
                    dispatchable += 1;
                    None
                } else {
                    Some("matcher_unavailable".to_string())
                }
            };
            if let Some(reason) = reason {
                *waiting.entry(reason).or_default() += 1;
            }
        }
        Ok(Some(DispatchDemand {
            observed_at,
            active_foreground_runs: active.len(),
            dispatchable_foreground_leaves: dispatchable,
            waiting,
        }))
    }
}
