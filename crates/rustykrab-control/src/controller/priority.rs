//! Explicit human ordering of the scheduler's existing work graph.
use super::{batch::Batch, questions::invalid, Controller};
use crate::graph::{self, PriorityList, Snapshot};
use rustykrab_core::{
    work::{EventKind, WorkItemId},
    Error,
};
use rustykrab_store::WorkOp;
use std::collections::{HashMap, HashSet};

impl Controller {
    pub(super) async fn reorder_priority_locked(
        &self,
        revision: &str,
        ordered: &[WorkItemId],
        actor: &str,
    ) -> Result<PriorityList, Error> {
        if !actor.starts_with("user:") {
            return Err(Error::Auth(
                "only an authenticated human can reorder work".into(),
            ));
        }
        let snap = self.load().await?;
        let now = self.clock.now();
        let current = graph::priority_list(&snap, now);
        if current.revision != revision {
            return Err(invalid(
                "The priority list changed. Refresh it before moving work.",
            ));
        }
        let expected: HashSet<_> = current.entries.iter().map(|e| &e.item).collect();
        let supplied: HashSet<_> = ordered.iter().collect();
        if ordered.len() != current.entries.len()
            || supplied != expected
            || supplied.len() != ordered.len()
        {
            return Err(invalid(
                "The order must contain every current priority-list item exactly once.",
            ));
        }
        if current.entries.iter().map(|e| &e.item).eq(ordered.iter()) {
            return Ok(current);
        }
        // Place the reviewed order above older urgency values, including
        // parent urgency. Prerequisites still inherit the work's urgency.
        let max = snap
            .items()
            .iter()
            .map(|i| i.priority)
            .max()
            .unwrap_or(0)
            .max(0);
        let count =
            i32::try_from(ordered.len()).map_err(|_| invalid("Too many priority-list items."))?;
        let top = max
            .checked_add(count)
            .ok_or_else(|| invalid("The priority range is exhausted."))?;
        let priorities: HashMap<_, _> = ordered
            .iter()
            .enumerate()
            .map(|(i, id)| (id.clone(), top - i as i32))
            .collect();
        let mut items = snap.items().to_vec();
        for item in &mut items {
            if let Some(priority) = priorities.get(&item.id) {
                item.priority = *priority;
            }
        }
        let edges = snap
            .items()
            .iter()
            .flat_map(|i| snap.edges_held_by(&i.id).cloned())
            .collect();
        let fired: Vec<_> = snap
            .items()
            .iter()
            .filter(|i| snap.trigger_fired(&i.id, now))
            .map(|i| i.id.clone())
            .collect();
        let proposed = graph::priority_list(&Snapshot::new(items, edges).with_fired(fired), now);
        if !proposed.entries.iter().map(|e| &e.item).eq(ordered.iter()) {
            return Err(invalid(
                "Prerequisite work must stay ahead of the work it unblocks.",
            ));
        }
        let mut batch = Batch::new(snap, now);
        for entry in &proposed.entries {
            let previous = batch
                .snap
                .item(&entry.item)
                .expect("reviewed item exists")
                .priority;
            batch.ops.push(WorkOp::SetPriority {
                item: entry.item.clone(),
                expected: previous,
                priority: entry.base_priority,
            });
            batch.note(
                &entry.item,
                EventKind::Decision,
                actor,
                format!(
                    "Priority list position {} (priority {}); execution scope and holds unchanged",
                    entry.position, entry.base_priority
                ),
            );
        }
        self.store.work_apply(batch.ops).await?;
        Ok(proposed)
    }
}
