//! Bounded, read-only observations of work in one SQLite read transaction.
//! Aggregate counts cover every durable row, independent of the display limits.
use super::{ops, rows, Spend, Store, WorkStoreError};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use rustykrab_core::work::{Lease, WorkEvent, WorkItem};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorItem {
    pub item: WorkItem,
    pub lease: Option<Lease>,
    pub last_worker: Option<String>,
    pub last_activity: Option<DateTime<Utc>>,
    pub children: u64,
    pub spend: Spend,
    pub evidence_count: u64,
    pub verified_evidence_count: u64,
    /// Exact context supplied to the latest run; never a newly generated handoff.
    #[serde(default)]
    pub project_context: Option<serde_json::Value>,
    #[serde(default)]
    pub workspace: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorEvent {
    pub cursor: i64,
    pub event: WorkEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkMonitorSnapshot {
    pub captured_at: DateTime<Utc>,
    pub counts: BTreeMap<String, u64>,
    /// Every durable lease, even when its item is outside the display limit.
    pub active_by_worker: BTreeMap<String, u64>,
    pub blocked_reasons: BTreeMap<String, u64>,
    pub total_live: u64,
    pub total_archived: u64,
    pub items: Vec<MonitorItem>,
    pub items_truncated: bool,
    pub events: Vec<MonitorEvent>,
    /// Resume the existing SSE stream here; a concurrent later event is not lost.
    pub event_cursor: i64,
    pub pending_questions: u64,
    pub pending_notices: u64,
    /// Earliest due time, including intentionally delayed notices.
    pub oldest_pending_notice: Option<DateTime<Utc>>,
}

fn count(conn: &Connection, sql: &str) -> Result<u64, WorkStoreError> {
    Ok(conn.query_row(sql, [], |row| row.get(0))?)
}

fn groups(conn: &Connection, sql: &str) -> Result<BTreeMap<String, u64>, WorkStoreError> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

impl Store {
    pub async fn work_monitor_snapshot(
        &self,
        item_limit: usize,
        event_limit: usize,
    ) -> Result<WorkMonitorSnapshot, WorkStoreError> {
        let item_limit = item_limit.clamp(1, 500);
        let event_limit = event_limit.clamp(1, 200);
        self.work_tx(move |conn| {
            // Prioritize live runs and blocked work over completed history.
            let mut stmt = conn.prepare(&format!(
                "SELECT {} FROM work_items ORDER BY
                 CASE status WHEN 'running' THEN 0 WHEN 'leased' THEN 1
                 WHEN 'verifying' THEN 2 WHEN 'blocked' THEN 3 WHEN 'ready' THEN 4
                 WHEN 'queued' THEN 5 ELSE 6 END, updated_at DESC, id LIMIT ?1",
                rows::ITEM_COLUMNS
            ))?;
            let selected = stmt.query_map(params![item_limit as i64], rows::item_from_row)?
                .collect::<Result<Vec<_>, _>>()?;
            let mut items = Vec::with_capacity(selected.len());
            for item in selected {
                let lease = ops::get_lease(conn, &item.id)?;
                let (last_actor, last_time): (Option<String>, Option<String>) = conn.query_row(
                    "SELECT actor, at FROM work_item_events WHERE item = ?1 ORDER BY id DESC LIMIT 1",
                    params![item.id], |r| Ok((r.get(0)?, r.get(1)?))
                ).optional()?.unwrap_or_default();
                let last_worker = match &lease {
                    Some(lease) => Some(lease.worker.clone()),
                    None => conn.query_row(
                        "SELECT worker FROM work_lease_history WHERE item = ?1
                         ORDER BY released_at DESC LIMIT 1",
                        params![item.id], |r| r.get(0)
                    ).optional()?.or_else(|| last_actor.and_then(|a| a.strip_prefix("worker:").map(str::to_owned))),
                };
                let children = conn.query_row(
                    "SELECT COUNT(*) FROM work_items WHERE parent = ?1",
                    params![item.id], |r| r.get(0)
                )?;
                let (evidence_count, verified_evidence_count) = conn.query_row(
                    "SELECT COUNT(*), COALESCE(SUM(verified_by IS NOT NULL), 0)
                     FROM work_item_evidence WHERE item = ?1",
                    params![item.id], |r| Ok((r.get(0)?, r.get(1)?))
                )?;
                let spend = ops::spend_of(conn, &item.id)?;
                let project_context = latest_json(conn, &item.id, "project_context", Some("controller"))?;
                let workspace = latest_json(conn, &item.id, "workspace", None)?;
                items.push(MonitorItem {
                    item, lease, last_worker, last_activity: last_time.as_deref().and_then(rows::parse_ts),
                    children, spend, evidence_count, verified_evidence_count, project_context, workspace,
                });
            }
            let mut stmt = conn.prepare(&format!(
                "SELECT id, {} FROM work_item_events ORDER BY id DESC LIMIT ?1",
                rows::EVENT_COLUMNS
            ))?;
            let mut events = stmt.query_map(params![event_limit as i64], |r| Ok(MonitorEvent {
                cursor: r.get(0)?, event: rows::event_from_row(r)?
            }))?.collect::<Result<Vec<_>, _>>()?;
            events.reverse();
            let total_live = count(conn, "SELECT COUNT(*) FROM work_items")?;
            let oldest: Option<String> = conn.query_row(
                "SELECT MIN(COALESCE(not_before, created_at)) FROM work_outbox WHERE delivered_at IS NULL", [], |r| r.get(0)
            )?;
            Ok(WorkMonitorSnapshot {
                captured_at: Utc::now(),
                counts: groups(conn, "SELECT status, COUNT(*) FROM work_items GROUP BY status")?,
                blocked_reasons: groups(conn, "SELECT COALESCE(status_reason, 'unknown'), COUNT(*)
                    FROM work_items WHERE status = 'blocked' GROUP BY status_reason")?,
                active_by_worker: groups(conn, "SELECT worker, COUNT(*) FROM leases GROUP BY worker")?,
                total_live,
                total_archived: count(conn, "SELECT COUNT(*) FROM work_item_archive")?,
                items_truncated: total_live > items.len() as u64,
                items, events,
                event_cursor: ops::events_last_id(conn)?,
                pending_questions: count(conn, "SELECT COUNT(*) FROM questions WHERE status = 'open'")?,
                pending_notices: count(conn, "SELECT COUNT(*) FROM work_outbox WHERE delivered_at IS NULL")?,
                oldest_pending_notice: oldest.as_deref().and_then(rows::parse_ts),
            })
        }).await
    }
}

// A malformed durable record is an observation failure, never an empty context.
fn latest_json(
    conn: &Connection,
    item: &str,
    kind: &str,
    verifier: Option<&str>,
) -> Result<Option<serde_json::Value>, WorkStoreError> {
    let value: Option<String> = conn
        .query_row(
            "SELECT ref FROM work_item_evidence WHERE item = ?1 AND kind = ?2
         AND (?3 IS NULL OR verified_by = ?3)
         AND (?2 <> 'workspace' OR (verified_by IS NULL AND hash IS NOT NULL))
         ORDER BY id DESC LIMIT 1",
            params![item, kind, verifier],
            |r| r.get(0),
        )
        .optional()?;
    value
        .map(|s| serde_json::from_str(&s).map_err(|e| WorkStoreError::Storage(e.to_string())))
        .transpose()
}
