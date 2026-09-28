//! The evaluation's store-shaped sources, over `rustykrab-store`.

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use rustykrab_core::proposal::{MetricValue, ProposalOutcome, ReviewState};
use rustykrab_core::Result;
use rustykrab_store::{ProposalEvidence, ProposalRow, Store, WorkFilter};

use super::facts::{ItemRecord, WorkRecords};
use super::{EvaluationLedger, WorkRecordSource};

/// Work records from the store: the live rows, the archive's summary
/// lines, the event log and the facets.
#[derive(Clone)]
pub struct StoreWorkRecords {
    store: Store,
}

impl StoreWorkRecords {
    pub fn new(store: Store) -> Self {
        Self { store }
    }
}

#[async_trait]
impl WorkRecordSource for StoreWorkRecords {
    async fn records(&self, since: DateTime<Utc>) -> Result<WorkRecords> {
        let live = self
            .store
            .work_list(&WorkFilter {
                include_closed: true,
                ..WorkFilter::default()
            })
            .await?;
        let mut out = WorkRecords::default();
        for item in live {
            let relevant = item.created_at >= since
                || item.closed_at.is_none_or(|t| t >= since)
                || item.updated_at >= since;
            if !relevant {
                continue;
            }
            out.edges.extend(self.store.work_edges_of(&item.id).await?);
            out.items.push(ItemRecord {
                id: item.id,
                kind: item.kind,
                status: item.status,
                title: item.title,
                parent: item.parent,
                created_at: item.created_at,
                closed_at: item.closed_at,
                artifact_refs: item.artifact_refs,
                archived: false,
            });
        }
        for a in self.store.work_archive_list(None, Some(since)).await? {
            out.edges.extend(a.edges.iter().cloned());
            out.items.push(ItemRecord {
                id: a.id,
                kind: a.kind,
                status: a.status,
                title: a.title,
                parent: a.parent,
                // The archive keeps no creation time; its closing time is
                // the earliest it can vouch for.
                created_at: a.closed_at,
                closed_at: Some(a.closed_at),
                artifact_refs: Vec::new(),
                archived: true,
            });
        }
        out.events = self.store.work_events_since(since).await?;
        out.facets = self.store.work_facets_all().await?;
        Ok(out)
    }
}

/// Metrics and proposal records in the store's Phase 6 tables.
#[derive(Clone)]
pub struct StoreLedger {
    store: Store,
}

impl StoreLedger {
    pub fn new(store: Store) -> Self {
        Self { store }
    }
}

#[async_trait]
impl EvaluationLedger for StoreLedger {
    async fn latest_metrics(&self) -> Result<Vec<MetricValue>> {
        self.store.expectation_metrics_latest().await
    }

    async fn record_metrics(&self, pass: &str, values: &[MetricValue]) -> Result<()> {
        self.store.expectation_metrics_record(pass, values).await?;
        // A year of nightly passes is plenty to compare against.
        let horizon = values
            .first()
            .map(|v| v.computed_at)
            .unwrap_or_else(Utc::now)
            - chrono::Duration::days(400);
        self.store.expectation_metrics_prune(horizon).await?;
        Ok(())
    }

    async fn proposals(&self) -> Result<Vec<ProposalRow>> {
        self.store.proposals_list().await
    }

    async fn record_proposal(
        &self,
        row: &ProposalRow,
        evidence: &[ProposalEvidence],
    ) -> Result<()> {
        self.store.proposal_insert(row, evidence).await
    }

    async fn record_review(
        &self,
        item: &str,
        review: ReviewState,
        decided_by: &str,
        decided_at: DateTime<Utc>,
        code_item: Option<&str>,
        baseline: Option<f64>,
    ) -> Result<()> {
        self.store
            .proposal_record_review(item, review, decided_by, decided_at, code_item, baseline)
            .await
            .map(|_| ())
    }

    async fn record_outcome(
        &self,
        item: &str,
        outcome: ProposalOutcome,
        observed: Option<f64>,
        at: DateTime<Utc>,
    ) -> Result<()> {
        self.store
            .proposal_record_outcome(item, outcome, observed, at)
            .await
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustykrab_core::outcome::SignalClass;
    use rustykrab_core::proposal::{Criterion, ProposalBody};
    use rustykrab_core::work::{
        Budget, EventKind, ReviewTier, Status, Trigger, WorkEvent, WorkFacets, WorkItem, WorkKind,
        WorkerKind,
    };
    use rustykrab_store::WorkOp;

    fn open() -> Store {
        let dir = std::env::temp_dir().join(format!("rk-evaluate-{}", uuid::Uuid::new_v4()));
        Store::open(&dir, vec![3u8; 32]).expect("store opens")
    }

    fn item(id: &str, kind: WorkKind) -> WorkItem {
        let now = Utc::now();
        WorkItem {
            id: id.into(),
            kind,
            title: format!("title {id}"),
            objective: "o".into(),
            done_when: "d".into(),
            constraints: vec![],
            decisions_made: vec![],
            artifact_refs: vec![],
            required_tools: vec![],
            required_mcp_servers: vec![],
            worker_kind: WorkerKind::Any,
            writable_resources: vec![],
            parent: None,
            inputs_from: vec![],
            origin_conversation_id: None,
            trigger: Trigger::Now,
            preconditions: vec![],
            expires_at: None,
            budget: Budget::default(),
            priority: 0,
            status: Status::Queued,
            status_origin: None,
            plan_id: None,
            held_by: None,
            created_at: now,
            updated_at: now,
            closed_at: None,
        }
    }

    #[tokio::test]
    async fn records_and_ledger_round_trip_through_the_store() {
        let store = open();
        store
            .work_apply(vec![
                WorkOp::Insert(Box::new(item("p1", WorkKind::Proposal))),
                WorkOp::Facets {
                    item: "p1".into(),
                    facets: WorkFacets {
                        capability: None,
                        subject: Some("skill:cal".into()),
                        review_tier: Some(ReviewTier::Standard),
                    },
                },
                WorkOp::Insert(Box::new(item("w1", WorkKind::Personal))),
                WorkOp::Note(WorkEvent {
                    item: "w1".into(),
                    at: Utc::now(),
                    kind: EventKind::Warning,
                    from: None,
                    to: None,
                    actor: "controller".into(),
                    reason: Some("sequential_split: w1".into()),
                    upstream: None,
                    origin: None,
                    evidence_ref: None,
                }),
            ])
            .await
            .unwrap();
        let since = Utc::now() - chrono::Duration::days(1);
        let records = StoreWorkRecords::new(store.clone())
            .records(since)
            .await
            .unwrap();
        assert_eq!(records.items.len(), 2);
        assert_eq!(records.facets["p1"].subject.as_deref(), Some("skill:cal"));
        assert!(records.events.iter().any(|e| e.kind == EventKind::Warning));

        let ledger = StoreLedger::new(store);
        let row = ProposalRow {
            item: "p1".into(),
            subject: "skill:cal".into(),
            criterion: Criterion::SkillOutcome,
            metric: "skill:cal".into(),
            body: ProposalBody {
                criterion: Criterion::SkillOutcome,
                observed: "o".into(),
                expectation: "e".into(),
                subject: "skill:cal".into(),
                metric: "skill:cal".into(),
                expected_movement: "up".into(),
                evidence: vec![],
                counterexamples: vec![],
                risk: "r".into(),
                rollback: "b".into(),
                falsified_by: "f".into(),
                signal: SignalClass::Verifiable,
            },
            filed_by: "dreaming".into(),
            created_at: Utc::now(),
            review: ReviewState::Pending,
            decided_by: None,
            decided_at: None,
            code_item: None,
            baseline: None,
            outcome: ProposalOutcome::Pending,
            observed: None,
            outcome_at: None,
        };
        ledger.record_proposal(&row, &[]).await.unwrap();
        ledger
            .record_review(
                "p1",
                ReviewState::Accepted,
                "x",
                Utc::now(),
                Some("c1"),
                Some(0.1),
            )
            .await
            .unwrap();
        let rows = ledger.proposals().await.unwrap();
        assert_eq!(rows[0].review, ReviewState::Accepted);
        assert_eq!(rows[0].baseline, Some(0.1));
        assert!(ledger.latest_metrics().await.unwrap().is_empty());
    }
}
