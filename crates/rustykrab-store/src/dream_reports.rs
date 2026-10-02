//! Persistence for analysis passes — the record that the outer loop ran
//! (see `DREAMING.md`).
//!
//! Phase 1 is gated on "reports show real, actionable patterns". A report
//! that exists only as a log line cannot answer that: it is gone at the
//! next rotation, it cannot be diffed against last week's, and nothing but
//! a human with `grep` can tell whether the loop has been running at all.
//!
//! So a pass is written down. The row carries the two fields worth
//! querying — when, and what the readiness verdict was — with the whole
//! report kept alongside as JSON, because the shape of an analysis will
//! change and a schema per revision is not worth the migrations.

use std::sync::Arc;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::params;
use uuid::Uuid;

use rustykrab_core::Error;

use crate::with_conn;

/// Analysis passes retained. A pass every couple of minutes is a few
/// hundred rows a day, and only the recent ones inform a decision — but
/// enough history to see a trend is the entire point, so this is generous.
const MAX_DREAM_REPORTS: u32 = 2_000;

/// One persisted analysis pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredReport {
    pub id: Uuid,
    pub generated_at: DateTime<Utc>,
    /// The readiness verdict, denormalized so "has this ever been ready?"
    /// is a query rather than a JSON scan.
    pub readiness: String,
    pub total_records: u32,
    /// The human-readable digest, as the worker would have logged it.
    pub summary: String,
    /// The full report as JSON.
    pub report: String,
}

/// Handle for analysis-pass persistence, backed by SQLite.
#[derive(Clone)]
pub struct DreamReportStore {
    conn: Arc<Mutex<rusqlite::Connection>>,
}

impl DreamReportStore {
    pub(crate) fn new(conn: Arc<Mutex<rusqlite::Connection>>) -> Self {
        Self { conn }
    }

    /// Write one pass, pruning the oldest once the cap is exceeded.
    pub async fn record(
        &self,
        generated_at: DateTime<Utc>,
        readiness: &str,
        total_records: u32,
        summary: &str,
        report_json: &str,
    ) -> Result<Uuid, Error> {
        let id = Uuid::new_v4();
        let (readiness, summary, report_json) = (
            readiness.to_string(),
            summary.to_string(),
            report_json.to_string(),
        );

        with_conn(&self.conn, move |conn| {
            conn.execute(
                "INSERT INTO dream_reports
                    (id, generated_at, readiness, total_records, summary, report)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    id.to_string(),
                    generated_at.to_rfc3339(),
                    readiness,
                    total_records,
                    summary,
                    report_json,
                ],
            )
            .map_err(|e| Error::Storage(e.to_string()))?;

            conn.execute(
                "DELETE FROM dream_reports
                 WHERE id IN (
                     SELECT id FROM dream_reports
                     ORDER BY generated_at DESC
                     LIMIT -1 OFFSET ?1
                 )",
                params![MAX_DREAM_REPORTS],
            )
            .map_err(|e| Error::Storage(e.to_string()))?;

            Ok(id)
        })
        .await
    }

    /// The most recent passes, newest first.
    pub async fn recent(&self, limit: u32) -> Result<Vec<StoredReport>, Error> {
        with_conn(&self.conn, move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, generated_at, readiness, total_records, summary, report
                     FROM dream_reports
                     ORDER BY generated_at DESC
                     LIMIT ?1",
                )
                .map_err(|e| Error::Storage(e.to_string()))?;

            let rows = stmt
                .query_map(params![limit], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, u32>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                })
                .map_err(|e| Error::Storage(e.to_string()))?;

            let mut out = Vec::new();
            for row in rows {
                let (id, generated_at, readiness, total_records, summary, report) =
                    row.map_err(|e| Error::Storage(e.to_string()))?;
                out.push(StoredReport {
                    id: Uuid::parse_str(&id)
                        .map_err(|e| Error::Storage(format!("bad report id: {e}")))?,
                    generated_at: DateTime::parse_from_rfc3339(&generated_at)
                        .map_err(|e| Error::Storage(format!("bad report timestamp: {e}")))?
                        .with_timezone(&Utc),
                    readiness,
                    total_records,
                    summary,
                    report,
                });
            }
            Ok(out)
        })
        .await
    }

    pub async fn count(&self) -> Result<u32, Error> {
        with_conn(&self.conn, move |conn| {
            conn.query_row("SELECT COUNT(*) FROM dream_reports", [], |row| row.get(0))
                .map_err(|e| Error::Storage(e.to_string()))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> DreamReportStore {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::Store::run_migrations(&conn).unwrap();
        DreamReportStore::new(Arc::new(Mutex::new(conn)))
    }

    #[tokio::test]
    async fn a_pass_is_readable_after_it_is_written() {
        // The whole reason the table exists: a pass that ran must still be
        // answerable for later, not only visible in a log that rotates.
        let s = store();
        s.record(Utc::now(), "proxy_only", 42, "digest", r#"{"a":1}"#)
            .await
            .unwrap();

        let recent = s.recent(10).await.unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].readiness, "proxy_only");
        assert_eq!(recent[0].total_records, 42);
        assert_eq!(recent[0].summary, "digest");
        assert_eq!(recent[0].report, r#"{"a":1}"#);
    }

    #[tokio::test]
    async fn passes_come_back_newest_first() {
        let s = store();
        let old = Utc::now() - chrono::Duration::hours(2);
        let new = Utc::now();
        s.record(old, "insufficient_data", 1, "old", "{}")
            .await
            .unwrap();
        s.record(new, "ready", 2, "new", "{}").await.unwrap();

        let recent = s.recent(10).await.unwrap();
        assert_eq!(recent[0].summary, "new");
        assert_eq!(recent[1].summary, "old");
    }

    #[tokio::test]
    async fn history_is_capped() {
        // A pass every couple of minutes on a long-running daemon must not
        // turn the table into the largest thing in the database.
        let s = store();
        let base = Utc::now();
        for i in 0..(MAX_DREAM_REPORTS + 25) {
            s.record(
                base + chrono::Duration::seconds(i as i64),
                "insufficient_data",
                i,
                "d",
                "{}",
            )
            .await
            .unwrap();
        }
        assert_eq!(s.count().await.unwrap(), MAX_DREAM_REPORTS);

        // Pruning drops the oldest, so the newest pass is still there.
        let recent = s.recent(1).await.unwrap();
        assert_eq!(recent[0].total_records, MAX_DREAM_REPORTS + 24);
    }
}
