//! Writing a [`Batch`]: its evidence rows, then its ops and the outbox
//! notices they cause in one `work_apply`, then the in-memory side effects
//! (stopped runs, recurrence counts, approvals, rate-limit records).

use std::collections::HashSet;

use rustykrab_core::work::{ArtifactRef, Status, WorkItemId};
use rustykrab_core::Error;
use rustykrab_store::{OutboxDraft, WorkOp};

use super::batch::Batch;
use super::brief::{first_line, ERROR, SUMMARY};
use super::notice::{render, NoticeData};
use super::Controller;

/// What a written batch did, for the tick report.
#[derive(Debug, Default)]
pub(super) struct Written {
    pub transitions: usize,
    pub notices: usize,
    pub made_ready: Vec<WorkItemId>,
}

impl Controller {
    /// Write `b`, or return `None` without writing anything when it owes a
    /// notice to a root that already had one this tick (`noticed`): one
    /// message per parent per tick (6.6). The caller retries next tick.
    pub(super) async fn commit(
        &self,
        mut b: Batch,
        noticed: &mut HashSet<WorkItemId>,
    ) -> Result<Option<Written>, Error> {
        let roots = b.notice_roots();
        if roots.iter().any(|(root, _)| noticed.contains(root)) {
            return Ok(None);
        }
        if b.ops.is_empty() && b.evidence.is_empty() {
            return Ok(Some(Written::default()));
        }
        let mut written = Written::default();
        for op in &b.ops {
            if let WorkOp::Transition(t) = op {
                written.transitions += 1;
                if t.to == Status::Ready && !written.made_ready.contains(&t.item) {
                    written.made_ready.push(t.item.clone());
                }
            }
        }
        for (root, causes) in &roots {
            let data = self.notice_data(&b, root).await?;
            let body = render(&b.snap, root, &data, causes);
            let origin = causes.iter().find_map(|c| c.item()).map(str::to_string);
            b.ops.push(WorkOp::Outbox(OutboxDraft {
                parent: root.clone(),
                origin,
                channel: self.config.notice_channel.clone(),
                body,
            }));
            written.notices += 1;
        }
        for evidence in std::mem::take(&mut b.evidence) {
            self.store.work_evidence_add(evidence).await?;
        }
        self.store.work_apply(std::mem::take(&mut b.ops)).await?;

        let stopped = self.apply_in_memory(&mut b);
        // A stopped run spent what it spent: recorded like one that ended.
        for (id, run) in &stopped {
            self.record_run(id, run, "stopped").await;
        }
        noticed.extend(roots.into_iter().map(|(root, _)| root));
        Ok(Some(written))
    }

    /// The in-memory side of a written batch: stop the runs it revoked
    /// (returned, so their spend is recorded), count fingerprints, note
    /// approvals, supersedes, planning runs and landed rules.
    fn apply_in_memory(&self, b: &mut Batch) -> Vec<(WorkItemId, super::Run)> {
        let mut state = self.state();
        let mut stopped = Vec::new();
        for id in &b.revoke {
            if let Some(run) = state.runs.remove(id) {
                run.handle.abort();
                stopped.push((id.clone(), run));
            }
            state.finished.remove(id);
        }
        for fingerprint in &b.observe {
            state.recurrence.observe(fingerprint);
        }
        for id in &b.approved {
            state.approved.insert(id.clone(), true);
        }
        let horizon = b.now - self.config.supersede_window;
        state.supersedes.retain(|(_, at)| *at > horizon);
        for root in &b.superseded_under {
            state.supersedes.push((root.clone(), b.now));
        }
        for item in &b.planned {
            state.planned.insert(item.clone());
        }
        state.learned.extend(std::mem::take(&mut b.learned));
        stopped
    }

    /// The store reads a notice for `root` needs: ladders of what failed or
    /// is blocked, workers of what runs, evidence and summaries of what is
    /// done, merged with what `b` is about to write.
    async fn notice_data(&self, b: &Batch, root: &str) -> Result<NoticeData, Error> {
        let mut data = NoticeData::default();
        let mut ids = vec![root.to_string()];
        ids.extend(b.snap.descendants(root));
        for id in &ids {
            let Some(row) = b.snap.item(id) else {
                continue;
            };
            let leaf = !b.snap.has_children(id);
            match row.status {
                Status::Failed | Status::Blocked(_) => {
                    let ladder = match b.ladders.get(id) {
                        Some(l) => l.clone(),
                        None => self.ladder_of(row).await?,
                    };
                    data.ladders.insert(id.clone(), ladder);
                }
                Status::Leased | Status::Running | Status::Verifying if leaf => {
                    if let Some(w) = self.worker_of(id).await? {
                        data.workers.insert(id.clone(), w);
                    }
                }
                Status::Done if leaf => {
                    if let Some(w) = self.finished_by(id).await? {
                        data.workers.insert(id.clone(), w);
                    }
                    let stored = self.store.work_evidence_list(id).await?;
                    let pending = b.evidence.iter().filter(|e| e.item == *id);
                    let mut refs: Vec<ArtifactRef> = Vec::new();
                    for ev in stored.iter().chain(pending) {
                        if ev.kind == SUMMARY {
                            data.summaries.insert(id.clone(), first_line(&ev.reference));
                        } else if ev.kind != ERROR && ev.verified_by.is_some() {
                            let r = ArtifactRef {
                                kind: ev.kind.clone(),
                                value: ev.reference.clone(),
                            };
                            if !refs.contains(&r) {
                                refs.push(r);
                            }
                        }
                    }
                    data.evidence.insert(id.clone(), refs);
                }
                _ => {}
            }
        }
        Ok(data)
    }

    /// The worker a done leaf ran on: its live lease while the batch that
    /// closes it is being written, else its last `lease` event.
    async fn finished_by(&self, item: &str) -> Result<Option<String>, Error> {
        if let Some(w) = self.worker_of(item).await? {
            return Ok(Some(w));
        }
        Ok(self
            .store
            .work_events(item)
            .await?
            .iter()
            .rev()
            .find(|e| e.kind == rustykrab_core::work::EventKind::Lease)
            .map(|e| {
                e.actor
                    .strip_prefix("worker:")
                    .unwrap_or(&e.actor)
                    .to_string()
            }))
    }
}
