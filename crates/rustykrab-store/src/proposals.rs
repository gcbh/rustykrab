//! Evaluation, proposals and the review surface: the durable half of the
//! control plan's Phase 6 (`docs/plans/control-layer-and-worker-fleet.md`,
//! sections 1.1, 10, 11 and 13).
//!
//! Five tables, each keyed on a work item id without a foreign key, so
//! what they record outlives the item's compaction into the archive, as
//! the event log does:
//!
//! - `work_item_facets`: what the review surface needs beside the row, a
//!   `capability` item's mode and a `proposal`'s subject and review tier.
//!   Written by the controller in the transaction that files the item
//!   ([`crate::WorkOp::Facets`]), so an item never exists without them.
//! - `proposals` and `proposal_evidence`: dreaming's record of a proposal
//!   it filed, its section 10 body, the records that justified it, the
//!   review decision and what probation found. `proposal_evidence` is the
//!   join of section 13 to `outcome_records`, `dream_reports` and the work
//!   items and events a proposal cites.
//! - `expectation_metrics`: every computed section 1.1 metric, one row per
//!   metric per pass, so a regression is a comparison of two passes.
//! - `work_projections`: which issue an item was projected to, and the
//!   digest of the fields last written, so a pass can tell its own writes
//!   from a hand edit.
//!
//! Like the rest of the work tables, the API is `impl Store` methods.

use std::collections::HashMap;

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use rustykrab_core::proposal::{
    Criterion, MetricValue, ProposalBody, ProposalOutcome, ReviewState,
};
use rustykrab_core::work::{CapabilityMode, ReviewTier, WorkFacets, WorkItemId};
use rustykrab_core::Error;

use crate::{with_conn, Store};

/// The DDL, run from `Store::run_migrations` in its own block.
pub(crate) fn migrate(conn: &Connection) -> Result<(), Error> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS work_item_facets (
            item        TEXT PRIMARY KEY,
            capability  TEXT,
            subject     TEXT,
            review_tier TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_work_item_facets_subject
            ON work_item_facets (subject) WHERE subject IS NOT NULL;

        CREATE TABLE IF NOT EXISTS proposals (
            item       TEXT PRIMARY KEY,
            subject    TEXT NOT NULL,
            criterion  TEXT NOT NULL,
            metric     TEXT NOT NULL,
            body       TEXT NOT NULL,
            filed_by   TEXT NOT NULL,
            created_at TEXT NOT NULL,
            review     TEXT NOT NULL DEFAULT 'pending',
            decided_by TEXT,
            decided_at TEXT,
            code_item  TEXT,
            baseline   REAL,
            outcome    TEXT NOT NULL DEFAULT 'pending',
            observed   REAL,
            outcome_at TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_proposals_subject
            ON proposals (subject, review);
        CREATE INDEX IF NOT EXISTS idx_proposals_created
            ON proposals (created_at);

        CREATE TABLE IF NOT EXISTS proposal_evidence (
            proposal TEXT NOT NULL,
            source   TEXT NOT NULL,
            ref      TEXT NOT NULL,
            PRIMARY KEY (proposal, source, ref)
        );

        CREATE TABLE IF NOT EXISTS expectation_metrics (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            pass        TEXT NOT NULL,
            name        TEXT NOT NULL,
            value       REAL NOT NULL,
            sample      INTEGER NOT NULL,
            data        TEXT NOT NULL,
            computed_at TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_expectation_metrics_pass
            ON expectation_metrics (pass);
        CREATE INDEX IF NOT EXISTS idx_expectation_metrics_name
            ON expectation_metrics (name, computed_at);

        CREATE TABLE IF NOT EXISTS work_projections (
            item         TEXT NOT NULL,
            surface      TEXT NOT NULL,
            external_id  TEXT NOT NULL,
            url          TEXT,
            digest       TEXT NOT NULL,
            projected_at TEXT NOT NULL,
            last_comment TEXT,
            PRIMARY KEY (item, surface)
        );
        ",
    )
    .map_err(storage)
}

fn storage(e: impl std::fmt::Display) -> Error {
    Error::Storage(e.to_string())
}

fn ts(t: &DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn parse_ts(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

// ── facets ─────────────────────────────────────────────────────────────

/// Write an item's facets, replacing what it had. Called inside a
/// `work_apply` transaction for [`crate::WorkOp::Facets`].
pub(crate) fn put_facets(conn: &Connection, item: &str, f: &WorkFacets) -> Result<(), Error> {
    conn.execute(
        "INSERT INTO work_item_facets (item, capability, subject, review_tier)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(item) DO UPDATE SET
             capability = excluded.capability,
             subject = excluded.subject,
             review_tier = excluded.review_tier",
        params![
            item,
            f.capability.map(|c| c.as_str()),
            f.subject,
            f.review_tier.map(|t| t.as_str()),
        ],
    )
    .map_err(storage)?;
    Ok(())
}

fn facets_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(String, WorkFacets)> {
    let capability: Option<String> = row.get(1)?;
    let tier: Option<String> = row.get(3)?;
    Ok((
        row.get(0)?,
        WorkFacets {
            capability: capability.as_deref().and_then(CapabilityMode::parse),
            subject: row.get(2)?,
            review_tier: tier.as_deref().map(ReviewTier::parse),
        },
    ))
}

// ── proposals ──────────────────────────────────────────────────────────

/// One `proposals` row: dreaming's record of a proposal it filed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProposalRow {
    /// The `proposal` work item.
    pub item: WorkItemId,
    pub subject: String,
    pub criterion: Criterion,
    pub metric: String,
    pub body: ProposalBody,
    /// `dreaming`, or the actor that filed it.
    pub filed_by: String,
    pub created_at: DateTime<Utc>,
    pub review: ReviewState,
    pub decided_by: Option<String>,
    pub decided_at: Option<DateTime<Utc>>,
    /// The `code` item an acceptance filed.
    pub code_item: Option<WorkItemId>,
    /// The named metric's value when the proposal was accepted.
    pub baseline: Option<f64>,
    pub outcome: ProposalOutcome,
    /// The named metric's value when probation concluded.
    pub observed: Option<f64>,
    pub outcome_at: Option<DateTime<Utc>>,
}

/// One cited record: `outcome_record`, `dream_report`, `work_item`,
/// `work_event`, `metric`, `routing_record` or `question`, and its id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalEvidence {
    pub source: String,
    pub reference: String,
}

const PROPOSAL_COLUMNS: &str = "item, subject, criterion, metric, body, filed_by, created_at, \
     review, decided_by, decided_at, code_item, baseline, outcome, observed, outcome_at";

fn proposal_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Option<ProposalRow>> {
    let criterion: String = row.get(2)?;
    let body: String = row.get(4)?;
    let created: String = row.get(6)?;
    let review: String = row.get(7)?;
    let decided_at: Option<String> = row.get(9)?;
    let outcome: String = row.get(12)?;
    let outcome_at: Option<String> = row.get(14)?;
    // A row whose criterion or body a newer build wrote in a shape this one
    // cannot read is skipped rather than guessed at: nothing is filed or
    // executed from it.
    let (Some(criterion), Ok(body)) = (
        Criterion::parse(&criterion),
        serde_json::from_str::<ProposalBody>(&body),
    ) else {
        return Ok(None);
    };
    Ok(Some(ProposalRow {
        item: row.get(0)?,
        subject: row.get(1)?,
        criterion,
        metric: row.get(3)?,
        body,
        filed_by: row.get(5)?,
        created_at: parse_ts(&created).unwrap_or_default(),
        review: ReviewState::parse(&review),
        decided_by: row.get(8)?,
        decided_at: decided_at.as_deref().and_then(parse_ts),
        code_item: row.get(10)?,
        baseline: row.get(11)?,
        outcome: ProposalOutcome::parse(&outcome),
        observed: row.get(13)?,
        outcome_at: outcome_at.as_deref().and_then(parse_ts),
    }))
}

// ── metrics ────────────────────────────────────────────────────────────

/// The newest pass's metric rows.
fn latest_metrics(conn: &Connection) -> Result<Vec<MetricValue>, Error> {
    let pass: Option<String> = conn
        .query_row(
            "SELECT pass FROM expectation_metrics ORDER BY id DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(storage)?;
    let Some(pass) = pass else {
        return Ok(Vec::new());
    };
    let mut stmt = conn
        .prepare("SELECT data FROM expectation_metrics WHERE pass = ?1 ORDER BY id")
        .map_err(storage)?;
    let rows = stmt
        .query_map([pass], |row| row.get::<_, String>(0))
        .map_err(storage)?;
    let mut out = Vec::new();
    for row in rows {
        let data = row.map_err(storage)?;
        // A row another build wrote in a shape this one cannot read is left
        // out: a metric missing from a pass is honest, a guessed one is not.
        if let Ok(value) = serde_json::from_str::<MetricValue>(&data) {
            out.push(value);
        }
    }
    Ok(out)
}

// ── projections ────────────────────────────────────────────────────────

/// One `work_projections` row: the issue an item was projected to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionRow {
    pub item: WorkItemId,
    /// `github`, or whichever adapter wrote it.
    pub surface: String,
    /// The issue's number (or the surface's own id).
    pub external_id: String,
    pub url: Option<String>,
    /// A digest of the fields last written, so a pass rewrites only what
    /// changed or drifted.
    pub digest: String,
    pub projected_at: DateTime<Utc>,
    /// The newest comment already read back, so a decision in a comment is
    /// applied once.
    pub last_comment: Option<String>,
}

fn projection_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProjectionRow> {
    let at: String = row.get(5)?;
    Ok(ProjectionRow {
        item: row.get(0)?,
        surface: row.get(1)?,
        external_id: row.get(2)?,
        url: row.get(3)?,
        digest: row.get(4)?,
        projected_at: parse_ts(&at).unwrap_or_default(),
        last_comment: row.get(6)?,
    })
}

const PROJECTION_COLUMNS: &str =
    "item, surface, external_id, url, digest, projected_at, last_comment";

// ── the API ────────────────────────────────────────────────────────────

impl Store {
    /// An item's review facets, if it has any.
    pub async fn work_facets_get(&self, item: &str) -> Result<Option<WorkFacets>, Error> {
        let item = item.to_string();
        with_conn(&self.conn, move |conn| {
            conn.query_row(
                "SELECT item, capability, subject, review_tier
                 FROM work_item_facets WHERE item = ?1",
                [item],
                facets_from_row,
            )
            .optional()
            .map(|row| row.map(|(_, f)| f))
            .map_err(storage)
        })
        .await
    }

    /// Every item's facets, by item.
    pub async fn work_facets_all(&self) -> Result<HashMap<WorkItemId, WorkFacets>, Error> {
        with_conn(&self.conn, move |conn| {
            let mut stmt = conn
                .prepare("SELECT item, capability, subject, review_tier FROM work_item_facets")
                .map_err(storage)?;
            let rows = stmt.query_map([], facets_from_row).map_err(storage)?;
            rows.collect::<Result<HashMap<_, _>, _>>().map_err(storage)
        })
        .await
    }

    /// Record a proposal dreaming filed, with the records it cites, in one
    /// transaction.
    pub async fn proposal_insert(
        &self,
        row: &ProposalRow,
        evidence: &[ProposalEvidence],
    ) -> Result<(), Error> {
        let row = row.clone();
        let evidence = evidence.to_vec();
        let body = serde_json::to_string(&row.body).map_err(storage)?;
        with_conn(&self.conn, move |conn| {
            let tx = conn.unchecked_transaction().map_err(storage)?;
            tx.execute(
                "INSERT INTO proposals (item, subject, criterion, metric, body, filed_by,
                     created_at, review, decided_by, decided_at, code_item, baseline,
                     outcome, observed, outcome_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                params![
                    row.item,
                    row.subject,
                    row.criterion.as_str(),
                    row.metric,
                    body,
                    row.filed_by,
                    ts(&row.created_at),
                    row.review.as_str(),
                    row.decided_by,
                    row.decided_at.as_ref().map(ts),
                    row.code_item,
                    row.baseline,
                    row.outcome.as_str(),
                    row.observed,
                    row.outcome_at.as_ref().map(ts),
                ],
            )
            .map_err(storage)?;
            for e in &evidence {
                tx.execute(
                    "INSERT OR IGNORE INTO proposal_evidence (proposal, source, ref)
                     VALUES (?1, ?2, ?3)",
                    params![row.item, e.source, e.reference],
                )
                .map_err(storage)?;
            }
            tx.commit().map_err(storage)
        })
        .await
    }

    pub async fn proposal_get(&self, item: &str) -> Result<Option<ProposalRow>, Error> {
        let item = item.to_string();
        with_conn(&self.conn, move |conn| {
            conn.query_row(
                &format!("SELECT {PROPOSAL_COLUMNS} FROM proposals WHERE item = ?1"),
                [item],
                proposal_from_row,
            )
            .optional()
            .map(Option::flatten)
            .map_err(storage)
        })
        .await
    }

    /// Every proposal record, oldest first.
    pub async fn proposals_list(&self) -> Result<Vec<ProposalRow>, Error> {
        with_conn(&self.conn, move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {PROPOSAL_COLUMNS} FROM proposals ORDER BY created_at, item"
                ))
                .map_err(storage)?;
            let rows = stmt.query_map([], proposal_from_row).map_err(storage)?;
            let mut out = Vec::new();
            for row in rows {
                if let Some(row) = row.map_err(storage)? {
                    out.push(row);
                }
            }
            Ok(out)
        })
        .await
    }

    /// The records a proposal cites.
    pub async fn proposal_evidence(&self, item: &str) -> Result<Vec<ProposalEvidence>, Error> {
        let item = item.to_string();
        with_conn(&self.conn, move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT source, ref FROM proposal_evidence WHERE proposal = ?1
                     ORDER BY source, ref",
                )
                .map_err(storage)?;
            let rows = stmt
                .query_map([item], |row| {
                    Ok(ProposalEvidence {
                        source: row.get(0)?,
                        reference: row.get(1)?,
                    })
                })
                .map_err(storage)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(storage)
        })
        .await
    }

    /// Record the review decision on a proposal. Returns whether a row was
    /// there to update (a proposal a human filed over REST has none).
    pub async fn proposal_record_review(
        &self,
        item: &str,
        review: ReviewState,
        decided_by: &str,
        decided_at: DateTime<Utc>,
        code_item: Option<&str>,
        baseline: Option<f64>,
    ) -> Result<bool, Error> {
        let item = item.to_string();
        let decided_by = decided_by.to_string();
        let code_item = code_item.map(str::to_string);
        with_conn(&self.conn, move |conn| {
            let n = conn
                .execute(
                    "UPDATE proposals
                        SET review = ?2, decided_by = ?3, decided_at = ?4,
                            code_item = COALESCE(?5, code_item),
                            baseline = COALESCE(?6, baseline)
                      WHERE item = ?1",
                    params![
                        item,
                        review.as_str(),
                        decided_by,
                        ts(&decided_at),
                        code_item,
                        baseline
                    ],
                )
                .map_err(storage)?;
            Ok(n > 0)
        })
        .await
    }

    /// Record what probation found. Returns whether a row was updated.
    pub async fn proposal_record_outcome(
        &self,
        item: &str,
        outcome: ProposalOutcome,
        observed: Option<f64>,
        at: DateTime<Utc>,
    ) -> Result<bool, Error> {
        let item = item.to_string();
        with_conn(&self.conn, move |conn| {
            let n = conn
                .execute(
                    "UPDATE proposals SET outcome = ?2, observed = ?3, outcome_at = ?4
                      WHERE item = ?1",
                    params![item, outcome.as_str(), observed, ts(&at)],
                )
                .map_err(storage)?;
            Ok(n > 0)
        })
        .await
    }

    /// Record one pass's metrics under `pass`, in one transaction.
    pub async fn expectation_metrics_record(
        &self,
        pass: &str,
        values: &[MetricValue],
    ) -> Result<(), Error> {
        let pass = pass.to_string();
        let mut rows = Vec::with_capacity(values.len());
        for v in values {
            rows.push((
                v.name.clone(),
                v.value,
                v.sample,
                serde_json::to_string(v).map_err(storage)?,
                ts(&v.computed_at),
            ));
        }
        with_conn(&self.conn, move |conn| {
            let tx = conn.unchecked_transaction().map_err(storage)?;
            for (name, value, sample, data, at) in &rows {
                tx.execute(
                    "INSERT INTO expectation_metrics (pass, name, value, sample, data, computed_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![pass, name, value, sample, data, at],
                )
                .map_err(storage)?;
            }
            tx.commit().map_err(storage)
        })
        .await
    }

    /// The newest pass's metrics, in the order they were computed; empty
    /// before the first pass.
    pub async fn expectation_metrics_latest(&self) -> Result<Vec<MetricValue>, Error> {
        with_conn(&self.conn, latest_metrics).await
    }

    /// Drop metric rows computed before `before`, keeping the newest pass
    /// whatever its age. Returns the rows removed.
    pub async fn expectation_metrics_prune(&self, before: DateTime<Utc>) -> Result<usize, Error> {
        with_conn(&self.conn, move |conn| {
            conn.execute(
                "DELETE FROM expectation_metrics
                  WHERE computed_at < ?1
                    AND pass <> (SELECT pass FROM expectation_metrics ORDER BY id DESC LIMIT 1)",
                [ts(&before)],
            )
            .map_err(storage)
        })
        .await
    }

    pub async fn work_projection_get(
        &self,
        item: &str,
        surface: &str,
    ) -> Result<Option<ProjectionRow>, Error> {
        let item = item.to_string();
        let surface = surface.to_string();
        with_conn(&self.conn, move |conn| {
            conn.query_row(
                &format!(
                    "SELECT {PROJECTION_COLUMNS} FROM work_projections
                      WHERE item = ?1 AND surface = ?2"
                ),
                params![item, surface],
                projection_from_row,
            )
            .optional()
            .map_err(storage)
        })
        .await
    }

    /// Every item projected to `surface`, by item.
    pub async fn work_projections_all(
        &self,
        surface: &str,
    ) -> Result<HashMap<WorkItemId, ProjectionRow>, Error> {
        let surface = surface.to_string();
        with_conn(&self.conn, move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {PROJECTION_COLUMNS} FROM work_projections WHERE surface = ?1"
                ))
                .map_err(storage)?;
            let rows = stmt
                .query_map([surface], projection_from_row)
                .map_err(storage)?;
            let mut out = HashMap::new();
            for row in rows {
                let row = row.map_err(storage)?;
                out.insert(row.item.clone(), row);
            }
            Ok(out)
        })
        .await
    }

    /// Record (or replace) the issue an item is projected to.
    pub async fn work_projection_put(&self, row: &ProjectionRow) -> Result<(), Error> {
        let row = row.clone();
        with_conn(&self.conn, move |conn| {
            conn.execute(
                "INSERT INTO work_projections
                     (item, surface, external_id, url, digest, projected_at, last_comment)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(item, surface) DO UPDATE SET
                     external_id = excluded.external_id,
                     url = excluded.url,
                     digest = excluded.digest,
                     projected_at = excluded.projected_at,
                     last_comment = excluded.last_comment",
                params![
                    row.item,
                    row.surface,
                    row.external_id,
                    row.url,
                    row.digest,
                    ts(&row.projected_at),
                    row.last_comment,
                ],
            )
            .map_err(storage)?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustykrab_core::outcome::SignalClass;
    use rustykrab_core::proposal::{Direction, UNKNOWN_ERROR_RATE};
    use rustykrab_core::work::{
        ArtifactRef, Budget, Status, Trigger, WorkItem, WorkKind, WorkerKind,
    };

    fn store() -> Store {
        Store::open_in_memory()
    }

    fn body(subject: &str) -> ProposalBody {
        ProposalBody {
            criterion: Criterion::SkillOutcome,
            observed: "six of six verified runs failed".into(),
            expectation: "Finish it correctly".into(),
            subject: subject.into(),
            metric: format!("{subject}:success_rate"),
            expected_movement: "up".into(),
            evidence: vec![ArtifactRef {
                kind: "outcome_record".into(),
                value: "r1".into(),
            }],
            counterexamples: vec![],
            risk: "low".into(),
            rollback: "revert if it drops".into(),
            falsified_by: "the next ten runs".into(),
            signal: SignalClass::Verifiable,
        }
    }

    fn row(item: &str, subject: &str) -> ProposalRow {
        ProposalRow {
            item: item.into(),
            subject: subject.into(),
            criterion: Criterion::SkillOutcome,
            metric: format!("{subject}:success_rate"),
            body: body(subject),
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
        }
    }

    fn metric(name: &str, value: f64) -> MetricValue {
        MetricValue {
            name: name.into(),
            expectation: "Know what went wrong".into(),
            direction: Direction::ToZero,
            unit: "rate".into(),
            signal: SignalClass::Verifiable,
            value,
            numerator: value,
            denominator: 1.0,
            sample: 1,
            computed_at: Utc::now(),
            window_days: 7,
            breakdown: vec![],
        }
    }

    fn item(id: &str, kind: WorkKind) -> WorkItem {
        let now = Utc::now();
        WorkItem {
            id: id.into(),
            kind,
            title: format!("item {id}"),
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
    async fn facets_are_written_with_the_item_in_one_batch() {
        let s = store();
        let facets = WorkFacets {
            capability: None,
            subject: Some("controller".into()),
            review_tier: Some(ReviewTier::Highest),
        };
        s.work_apply(vec![
            crate::WorkOp::Insert(Box::new(item("p1", WorkKind::Proposal))),
            crate::WorkOp::Facets {
                item: "p1".into(),
                facets: facets.clone(),
            },
        ])
        .await
        .unwrap();
        assert_eq!(s.work_facets_get("p1").await.unwrap(), Some(facets));
        assert_eq!(s.work_facets_get("nope").await.unwrap(), None);
        // A batch that fails writes no facets either.
        let failed = s
            .work_apply(vec![
                crate::WorkOp::Facets {
                    item: "p2".into(),
                    facets: WorkFacets {
                        capability: Some(CapabilityMode::Build),
                        ..WorkFacets::default()
                    },
                },
                crate::WorkOp::Insert(Box::new(item("p1", WorkKind::Proposal))),
            ])
            .await;
        assert!(failed.is_err());
        assert_eq!(s.work_facets_get("p2").await.unwrap(), None);
        assert_eq!(s.work_facets_all().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn proposals_record_their_evidence_review_and_outcome() {
        let s = store();
        let evidence = vec![
            ProposalEvidence {
                source: "outcome_record".into(),
                reference: "r1".into(),
            },
            ProposalEvidence {
                source: "work_item".into(),
                reference: "w1".into(),
            },
        ];
        s.proposal_insert(&row("p1", "skill:cal"), &evidence)
            .await
            .unwrap();
        assert_eq!(s.proposal_evidence("p1").await.unwrap(), evidence);
        let now = Utc::now();
        assert!(s
            .proposal_record_review(
                "p1",
                ReviewState::Accepted,
                "reviewer:github:ada",
                now,
                Some("c1"),
                Some(0.25)
            )
            .await
            .unwrap());
        assert!(!s
            .proposal_record_review("ghost", ReviewState::Declined, "x", now, None, None)
            .await
            .unwrap());
        s.proposal_record_outcome("p1", ProposalOutcome::NotMoved, Some(0.25), now)
            .await
            .unwrap();
        let got = s.proposal_get("p1").await.unwrap().unwrap();
        assert_eq!(got.review, ReviewState::Accepted);
        assert_eq!(got.code_item.as_deref(), Some("c1"));
        assert_eq!(got.baseline, Some(0.25));
        assert_eq!(got.outcome, ProposalOutcome::NotMoved);
        assert_eq!(got.body, body("skill:cal"));
        assert_eq!(s.proposals_list().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_latest_metrics_are_the_newest_pass_only() {
        let s = store();
        assert!(s.expectation_metrics_latest().await.unwrap().is_empty());
        s.expectation_metrics_record("a", &[metric(UNKNOWN_ERROR_RATE, 0.1)])
            .await
            .unwrap();
        s.expectation_metrics_record(
            "b",
            &[
                metric(UNKNOWN_ERROR_RATE, 0.3),
                metric("typed_error_rate", 0.7),
            ],
        )
        .await
        .unwrap();
        let latest = s.expectation_metrics_latest().await.unwrap();
        assert_eq!(latest.len(), 2);
        assert_eq!(latest[0].value, 0.3);
        // Pruning keeps the newest pass whatever its age.
        let removed = s
            .expectation_metrics_prune(Utc::now() + chrono::Duration::days(1))
            .await
            .unwrap();
        assert_eq!(removed, 1);
        assert_eq!(s.expectation_metrics_latest().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn projections_are_kept_per_item_and_surface() {
        let s = store();
        let mut p = ProjectionRow {
            item: "i1".into(),
            surface: "github".into(),
            external_id: "7".into(),
            url: Some("https://github.com/o/r/issues/7".into()),
            digest: "abc".into(),
            projected_at: Utc::now(),
            last_comment: None,
        };
        s.work_projection_put(&p).await.unwrap();
        p.digest = "def".into();
        p.last_comment = Some("42".into());
        s.work_projection_put(&p).await.unwrap();
        let got = s
            .work_projection_get("i1", "github")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.digest, "def");
        assert_eq!(got.last_comment.as_deref(), Some("42"));
        assert!(s
            .work_projection_get("i1", "linear")
            .await
            .unwrap()
            .is_none());
        assert_eq!(s.work_projections_all("github").await.unwrap().len(), 1);
    }
}
