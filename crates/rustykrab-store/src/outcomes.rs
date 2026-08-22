//! Persistence for outcome records — the Monitor stage of the
//! self-improvement outer loop (see `DREAMING.md`).
//!
//! Records are written once and never updated. Per-artifact tallies are
//! derived by aggregating them on read rather than maintained as counters,
//! so a change in how outcomes are scored can be recomputed from history
//! instead of re-collected. That costs a `GROUP BY` and buys the ability to
//! be wrong about scoring without losing the underlying evidence.

use std::sync::Arc;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::params;
use uuid::Uuid;

use rustykrab_core::outcome::{
    Attribution, AttributionKind, ExecutionCounters, OutcomeRecord, OutcomeTally, OutcomeVerdict,
    SignalClass,
};
use rustykrab_core::Error;

use crate::with_conn;

/// Maximum retained outcome records. Older rows are pruned on insert so the
/// table cannot grow without bound on a long-running daemon. Sized to hold
/// a meaningful history for offline analysis without becoming a liability.
const MAX_OUTCOME_RECORDS: u32 = 50_000;

/// Handle for outcome-record persistence, backed by SQLite.
///
/// All methods run their rusqlite work on tokio's blocking pool via
/// `with_conn` so async workers never park on disk I/O.
#[derive(Clone)]
pub struct OutcomeStore {
    conn: Arc<Mutex<rusqlite::Connection>>,
}

impl OutcomeStore {
    pub(crate) fn new(conn: Arc<Mutex<rusqlite::Connection>>) -> Self {
        Self { conn }
    }

    /// Persist one outcome record and its attributions.
    ///
    /// The record and its attributions are written in a single transaction:
    /// an outcome with no attributions is not actionable, so a partial
    /// write would be worse than no write.
    pub async fn record(&self, record: &OutcomeRecord) -> Result<(), Error> {
        let r = record.clone();
        with_conn(&self.conn, move |conn| {
            let tx = conn
                .unchecked_transaction()
                .map_err(|e| Error::Storage(e.to_string()))?;

            tx.execute(
                "INSERT INTO outcome_records
                    (id, conversation_id, session_id, recorded_at, verdict, signal,
                     confidence, detail, tool_calls, tool_failures, iterations,
                     compactions, rustykrab_version)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    r.id.to_string(),
                    r.conversation_id.to_string(),
                    r.session_id.to_string(),
                    r.recorded_at.to_rfc3339(),
                    r.verdict.as_str(),
                    r.signal.as_str(),
                    r.confidence,
                    r.detail,
                    r.counters.tool_calls,
                    r.counters.tool_failures,
                    r.counters.iterations,
                    r.counters.compactions,
                    r.rustykrab_version,
                ],
            )
            .map_err(|e| Error::Storage(e.to_string()))?;

            {
                let mut stmt = tx
                    .prepare(
                        "INSERT OR IGNORE INTO outcome_attributions
                            (record_id, kind, target_id)
                         VALUES (?1, ?2, ?3)",
                    )
                    .map_err(|e| Error::Storage(e.to_string()))?;
                for attribution in &r.attributions {
                    stmt.execute(params![
                        r.id.to_string(),
                        attribution.kind.as_str(),
                        attribution.id,
                    ])
                    .map_err(|e| Error::Storage(e.to_string()))?;
                }
            }

            tx.commit().map_err(|e| Error::Storage(e.to_string()))?;
            Ok(())
        })
        .await?;

        self.prune().await
    }

    /// Drop the oldest records once the table exceeds its cap.
    ///
    /// Attributions are removed by `ON DELETE CASCADE`.
    async fn prune(&self) -> Result<(), Error> {
        with_conn(&self.conn, move |conn| {
            conn.execute(
                "DELETE FROM outcome_records
                 WHERE id IN (
                     SELECT id FROM outcome_records
                     ORDER BY recorded_at DESC
                     LIMIT -1 OFFSET ?1
                 )",
                params![MAX_OUTCOME_RECORDS],
            )
            .map_err(|e| Error::Storage(e.to_string()))?;
            Ok(())
        })
        .await
    }

    /// Aggregate helpful/harmful/ambiguous counts for one artifact.
    ///
    /// When `ground_truth_only` is set, judgements resting on proxy signals
    /// (implicit behaviour, model opinion) are excluded. A caller deciding
    /// something costly should ask for ground truth; a caller producing a
    /// report can use everything.
    pub async fn tally(
        &self,
        kind: AttributionKind,
        target_id: &str,
        ground_truth_only: bool,
    ) -> Result<OutcomeTally, Error> {
        let kind_str = kind.as_str().to_string();
        let target = target_id.to_string();

        with_conn(&self.conn, move |conn| {
            let sql = if ground_truth_only {
                "SELECT r.verdict, COUNT(*)
                 FROM outcome_records r
                 JOIN outcome_attributions a ON a.record_id = r.id
                 WHERE a.kind = ?1 AND a.target_id = ?2
                   AND r.signal IN ('verifiable', 'explicit')
                 GROUP BY r.verdict"
            } else {
                "SELECT r.verdict, COUNT(*)
                 FROM outcome_records r
                 JOIN outcome_attributions a ON a.record_id = r.id
                 WHERE a.kind = ?1 AND a.target_id = ?2
                 GROUP BY r.verdict"
            };

            let mut stmt = conn
                .prepare(sql)
                .map_err(|e| Error::Storage(e.to_string()))?;
            let rows = stmt
                .query_map(params![kind_str, target], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?))
                })
                .map_err(|e| Error::Storage(e.to_string()))?;

            let mut tally = OutcomeTally::default();
            for row in rows {
                let (verdict, count) = row.map_err(|e| Error::Storage(e.to_string()))?;
                match OutcomeVerdict::parse(&verdict) {
                    Some(OutcomeVerdict::Success) => tally.helpful += count,
                    Some(OutcomeVerdict::Failure) => tally.harmful += count,
                    Some(OutcomeVerdict::Ambiguous) => tally.ambiguous += count,
                    // An unrecognized verdict is a schema drift, not a
                    // silent zero — but a read path is the wrong place to
                    // fail, so it is skipped and left to the reporting
                    // stage to notice the totals don't add up.
                    None => {}
                }
            }
            Ok(tally)
        })
        .await
    }

    /// Every artifact of `kind` that has at least one recorded outcome,
    /// with its tally. Ordered by decisive-observation count descending, so
    /// the best-evidenced artifacts come first.
    pub async fn tallies_by_kind(
        &self,
        kind: AttributionKind,
        ground_truth_only: bool,
    ) -> Result<Vec<(String, OutcomeTally)>, Error> {
        let kind_str = kind.as_str().to_string();

        with_conn(&self.conn, move |conn| {
            let sql = if ground_truth_only {
                "SELECT a.target_id, r.verdict, COUNT(*)
                 FROM outcome_records r
                 JOIN outcome_attributions a ON a.record_id = r.id
                 WHERE a.kind = ?1 AND r.signal IN ('verifiable', 'explicit')
                 GROUP BY a.target_id, r.verdict"
            } else {
                "SELECT a.target_id, r.verdict, COUNT(*)
                 FROM outcome_records r
                 JOIN outcome_attributions a ON a.record_id = r.id
                 WHERE a.kind = ?1
                 GROUP BY a.target_id, r.verdict"
            };

            let mut stmt = conn
                .prepare(sql)
                .map_err(|e| Error::Storage(e.to_string()))?;
            let rows = stmt
                .query_map(params![kind_str], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, u32>(2)?,
                    ))
                })
                .map_err(|e| Error::Storage(e.to_string()))?;

            let mut by_target: std::collections::HashMap<String, OutcomeTally> =
                std::collections::HashMap::new();
            for row in rows {
                let (target, verdict, count) = row.map_err(|e| Error::Storage(e.to_string()))?;
                let tally = by_target.entry(target).or_default();
                match OutcomeVerdict::parse(&verdict) {
                    Some(OutcomeVerdict::Success) => tally.helpful += count,
                    Some(OutcomeVerdict::Failure) => tally.harmful += count,
                    Some(OutcomeVerdict::Ambiguous) => tally.ambiguous += count,
                    None => {}
                }
            }

            let mut out: Vec<(String, OutcomeTally)> = by_target.into_iter().collect();
            out.sort_by(|a, b| {
                b.1.decisive()
                    .cmp(&a.1.decisive())
                    .then_with(|| a.0.cmp(&b.0))
            });
            Ok(out)
        })
        .await
    }

    /// The most recent records, newest first, with their attributions.
    pub async fn recent(&self, limit: u32) -> Result<Vec<OutcomeRecord>, Error> {
        with_conn(&self.conn, move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, conversation_id, session_id, recorded_at, verdict, signal,
                            confidence, detail, tool_calls, tool_failures, iterations,
                            compactions, rustykrab_version
                     FROM outcome_records
                     ORDER BY recorded_at DESC
                     LIMIT ?1",
                )
                .map_err(|e| Error::Storage(e.to_string()))?;

            let rows = stmt
                .query_map(params![limit], row_to_record)
                .map_err(|e| Error::Storage(e.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| Error::Storage(e.to_string()))?;

            // Attach attributions per record.
            let mut attr_stmt = conn
                .prepare("SELECT kind, target_id FROM outcome_attributions WHERE record_id = ?1")
                .map_err(|e| Error::Storage(e.to_string()))?;

            let mut out = Vec::with_capacity(rows.len());
            for mut record in rows {
                let attributions = attr_stmt
                    .query_map(params![record.id.to_string()], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })
                    .map_err(|e| Error::Storage(e.to_string()))?
                    .filter_map(|r| r.ok())
                    .filter_map(|(kind, id)| {
                        AttributionKind::parse(&kind).map(|kind| Attribution { kind, id })
                    })
                    .collect();
                record.attributions = attributions;
                out.push(record);
            }
            Ok(out)
        })
        .await
    }

    /// Total number of records held.
    pub async fn count(&self) -> Result<u32, Error> {
        with_conn(&self.conn, move |conn| {
            conn.query_row("SELECT COUNT(*) FROM outcome_records", [], |row| row.get(0))
                .map_err(|e| Error::Storage(e.to_string()))
        })
        .await
    }
}

/// Lets the agent loop write outcomes without depending on this crate.
#[async_trait::async_trait]
impl rustykrab_core::outcome::OutcomeSink for OutcomeStore {
    async fn record_outcome(&self, record: OutcomeRecord) -> Result<(), Error> {
        self.record(&record).await
    }
}

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<OutcomeRecord> {
    let parse_uuid = |s: String| Uuid::parse_str(&s).unwrap_or_default();
    let recorded_at: String = row.get("recorded_at")?;

    Ok(OutcomeRecord {
        id: parse_uuid(row.get("id")?),
        conversation_id: parse_uuid(row.get("conversation_id")?),
        session_id: parse_uuid(row.get("session_id")?),
        recorded_at: DateTime::parse_from_rfc3339(&recorded_at)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now()),
        verdict: OutcomeVerdict::parse(&row.get::<_, String>("verdict")?)
            .unwrap_or(OutcomeVerdict::Ambiguous),
        signal: SignalClass::parse(&row.get::<_, String>("signal")?)
            .unwrap_or(SignalClass::Implicit),
        confidence: row.get("confidence")?,
        detail: row.get("detail")?,
        counters: ExecutionCounters {
            tool_calls: row.get("tool_calls")?,
            tool_failures: row.get("tool_failures")?,
            iterations: row.get("iterations")?,
            compactions: row.get("compactions")?,
        },
        attributions: Vec::new(),
        rustykrab_version: row.get("rustykrab_version")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustykrab_core::outcome::Attribution;

    fn test_store() -> OutcomeStore {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        // CASCADE from outcome_records to outcome_attributions only fires
        // with foreign keys enabled, matching what `Store::open` sets.
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        crate::Store::run_migrations(&conn).unwrap();
        OutcomeStore::new(Arc::new(Mutex::new(conn)))
    }

    fn record_with(
        verdict: OutcomeVerdict,
        signal: SignalClass,
        attributions: Vec<Attribution>,
    ) -> OutcomeRecord {
        OutcomeRecord::new(Uuid::new_v4(), Uuid::new_v4(), verdict, signal)
            .with_attributions(attributions)
    }

    #[tokio::test]
    async fn records_and_tallies_by_artifact() {
        let outcomes = test_store();
        let skill = Attribution::skill("calendar");

        outcomes
            .record(&record_with(
                OutcomeVerdict::Success,
                SignalClass::Verifiable,
                vec![skill.clone()],
            ))
            .await
            .unwrap();
        outcomes
            .record(&record_with(
                OutcomeVerdict::Failure,
                SignalClass::Verifiable,
                vec![skill.clone()],
            ))
            .await
            .unwrap();
        outcomes
            .record(&record_with(
                OutcomeVerdict::Success,
                SignalClass::Verifiable,
                vec![skill.clone()],
            ))
            .await
            .unwrap();

        let tally = outcomes
            .tally(AttributionKind::Skill, "calendar", false)
            .await
            .unwrap();
        assert_eq!(tally.helpful, 2);
        assert_eq!(tally.harmful, 1);
        assert_eq!(tally.success_rate(3), Some(2.0 / 3.0));
    }

    #[tokio::test]
    async fn ground_truth_filter_excludes_proxy_signals() {
        let outcomes = test_store();
        let skill = Attribution::skill("summarize");

        // One verifiable failure, two judge-based successes.
        outcomes
            .record(&record_with(
                OutcomeVerdict::Failure,
                SignalClass::Verifiable,
                vec![skill.clone()],
            ))
            .await
            .unwrap();
        for _ in 0..2 {
            outcomes
                .record(&record_with(
                    OutcomeVerdict::Success,
                    SignalClass::Judge,
                    vec![skill.clone()],
                ))
                .await
                .unwrap();
        }

        let all = outcomes
            .tally(AttributionKind::Skill, "summarize", false)
            .await
            .unwrap();
        assert_eq!(all.helpful, 2);
        assert_eq!(all.harmful, 1);

        // Judged successes must not launder a verifiable failure.
        let ground_truth = outcomes
            .tally(AttributionKind::Skill, "summarize", true)
            .await
            .unwrap();
        assert_eq!(ground_truth.helpful, 0);
        assert_eq!(ground_truth.harmful, 1);
    }

    #[tokio::test]
    async fn attributions_are_isolated_per_artifact() {
        let outcomes = test_store();
        let mem_id = Uuid::new_v4();

        outcomes
            .record(&record_with(
                OutcomeVerdict::Success,
                SignalClass::Explicit,
                vec![Attribution::skill("a"), Attribution::memory(mem_id)],
            ))
            .await
            .unwrap();

        let skill_tally = outcomes
            .tally(AttributionKind::Skill, "a", false)
            .await
            .unwrap();
        let memory_tally = outcomes
            .tally(AttributionKind::Memory, &mem_id.to_string(), false)
            .await
            .unwrap();
        let other = outcomes
            .tally(AttributionKind::Skill, "b", false)
            .await
            .unwrap();

        assert_eq!(skill_tally.helpful, 1);
        assert_eq!(memory_tally.helpful, 1);
        assert_eq!(other.decisive(), 0);
    }

    #[tokio::test]
    async fn recent_round_trips_records_with_attributions() {
        let outcomes = test_store();
        let counters = ExecutionCounters {
            tool_calls: 5,
            tool_failures: 2,
            iterations: 3,
            compactions: 1,
        };
        let record = OutcomeRecord::new(
            Uuid::new_v4(),
            Uuid::new_v4(),
            OutcomeVerdict::Failure,
            SignalClass::Implicit,
        )
        .with_confidence(0.4)
        .with_detail("two tools failed")
        .with_counters(counters)
        .with_attributions(vec![Attribution::tool("web_fetch")]);

        outcomes.record(&record).await.unwrap();

        let recent = outcomes.recent(10).await.unwrap();
        assert_eq!(recent.len(), 1);
        let got = &recent[0];
        assert_eq!(got.id, record.id);
        assert_eq!(got.verdict, OutcomeVerdict::Failure);
        assert_eq!(got.signal, SignalClass::Implicit);
        assert!((got.confidence - 0.4).abs() < f64::EPSILON);
        assert_eq!(got.detail.as_deref(), Some("two tools failed"));
        assert_eq!(got.counters, counters);
        assert_eq!(got.attributions, vec![Attribution::tool("web_fetch")]);
    }

    #[tokio::test]
    async fn tallies_by_kind_ranks_by_evidence() {
        let outcomes = test_store();

        for _ in 0..3 {
            outcomes
                .record(&record_with(
                    OutcomeVerdict::Success,
                    SignalClass::Verifiable,
                    vec![Attribution::skill("busy")],
                ))
                .await
                .unwrap();
        }
        outcomes
            .record(&record_with(
                OutcomeVerdict::Success,
                SignalClass::Verifiable,
                vec![Attribution::skill("quiet")],
            ))
            .await
            .unwrap();

        let tallies = outcomes
            .tallies_by_kind(AttributionKind::Skill, false)
            .await
            .unwrap();
        assert_eq!(tallies.len(), 2);
        assert_eq!(tallies[0].0, "busy");
        assert_eq!(tallies[0].1.helpful, 3);
        assert_eq!(tallies[1].0, "quiet");
    }

    #[tokio::test]
    async fn record_with_no_attributions_is_still_stored() {
        // Not actionable, but it is evidence about the signal itself —
        // a run nothing could be attributed to is worth being able to count.
        let outcomes = test_store();
        outcomes
            .record(&record_with(
                OutcomeVerdict::Ambiguous,
                SignalClass::Implicit,
                vec![],
            ))
            .await
            .unwrap();
        assert_eq!(outcomes.count().await.unwrap(), 1);
    }
}
