//! Comments use the existing append-only event ledger. They guide future runs;
//! they neither settle a consent question nor change a running acceptance contract.
use super::Controller;
use chrono::Utc;
use rustykrab_core::{
    work::{EventKind, WorkEvent},
    Error,
};
use rustykrab_store::QuestionFilter;
use rustykrab_tools::work_discuss::WorkDiscussion;

impl Controller {
    pub(super) async fn comment_locked(
        &self,
        item: &str,
        text: &str,
        actor: &str,
        run_item: Option<&str>,
    ) -> Result<WorkEvent, Error> {
        let text = text.trim();
        if text.is_empty() || text.len() > 4096 {
            return Err(super::questions::invalid(
                "A comment needs 1–4096 UTF-8 bytes.",
            ));
        }
        if self.store.work_get(item).await?.is_none() {
            return Err(Error::NotFound(format!("work item {item}")));
        }
        if let Some(held) = run_item {
            let lease = self.store.work_lease_get(item).await?;
            if held != item || !lease.is_some_and(|l| actor == format!("worker:{}", l.worker)) {
                return Err(super::questions::invalid(
                    "A worker may comment only on the item it currently holds.",
                ));
            }
        }
        let event = WorkEvent {
            item: item.into(),
            at: Utc::now(),
            kind: EventKind::Comment,
            actor: actor.into(),
            reason: Some(text.into()),
            from: None,
            to: None,
            upstream: None,
            origin: None,
            evidence_ref: None,
        };
        self.store.work_event_append(&event).await?;
        Ok(event)
    }

    pub(super) async fn discussion_view(
        &self,
        item: &str,
        offset: usize,
        limit: usize,
    ) -> Result<WorkDiscussion, Error> {
        if self.store.work_get(item).await?.is_none() {
            return Err(Error::NotFound(format!("work item {item}")));
        }
        let events: Vec<_> = self
            .store
            .work_events(item)
            .await?
            .into_iter()
            .filter(|e| e.kind == EventKind::Comment)
            .collect();
        let total = events.len();
        let activity = self
            .store
            .work_events(item)
            .await?
            .into_iter()
            .filter(|e| {
                !matches!(e.kind, EventKind::Comment | EventKind::Question) && e.reason.is_some()
            })
            .rev()
            .take(50)
            .collect();
        let comments = events
            .into_iter()
            .rev()
            .skip(offset)
            .take(limit.clamp(1, 100))
            .collect();
        let questions = self
            .store
            .questions_list(&QuestionFilter {
                item: Some(item.into()),
                ..Default::default()
            })
            .await?;
        Ok(WorkDiscussion {
            item: item.into(),
            comments,
            activity,
            questions,
            total_comments: total,
            offset,
        })
    }

    /// Root/ancestor guidance precedes item guidance. Freeze the full discussion
    /// in the lease brief; render only a bounded excerpt into the model prompt.
    pub(super) async fn discussion_for(&self, item: &str) -> Result<Vec<WorkEvent>, Error> {
        let mut ids = vec![item.to_string()];
        let mut parent = self.store.work_get(item).await?.and_then(|i| i.parent);
        while let Some(id) = parent {
            if ids.contains(&id) {
                break;
            }
            parent = self.store.work_get(&id).await?.and_then(|i| i.parent);
            ids.push(id);
        }
        ids.reverse();
        let mut comments = Vec::new();
        for id in ids {
            comments.extend(
                self.store
                    .work_events(&id)
                    .await?
                    .into_iter()
                    .filter(|e| e.kind == EventKind::Comment),
            );
        }
        Ok(comments)
    }
}
