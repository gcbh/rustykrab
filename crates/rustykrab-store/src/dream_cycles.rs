//! Persistence for dream cycles and their staged changes (see
//! `DREAMING.md`).
//!
//! This table *is* the safety property. A cycle records its whole
//! change-set here before touching anything, so:
//!
//! - staged work is durable but inert — a crash between staging and
//!   promoting leaves live state untouched;
//! - a promoted cycle can be undone, because what it did was written down
//!   rather than inferred afterwards.
//!
//! Changes are stored as JSON in the order they were planned, and applied
//! in that order. Reversal walks them backwards, so a create/invalidate
//! pair undoes in the opposite sequence to the one that applied it.

use std::sync::Arc;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::params;
use uuid::Uuid;

use rustykrab_core::dream::{CycleStatus, DreamCycle, StagedChange};
use rustykrab_core::Error;

use crate::with_conn;

/// Handle for dream-cycle persistence, backed by SQLite.
#[derive(Clone)]
pub struct DreamCycleStore {
    conn: Arc<Mutex<rusqlite::Connection>>,
}

impl DreamCycleStore {
    pub(crate) fn new(conn: Arc<Mutex<rusqlite::Connection>>) -> Self {
        Self { conn }
    }

    /// Record a cycle and its change-set, staged and inert.
    ///
    /// Written in one transaction: a cycle whose changes were only
    /// partially recorded could be promoted into a state it cannot
    /// describe, and therefore cannot reverse.
    pub async fn stage(&self, cycle: &DreamCycle, changes: &[StagedChange]) -> Result<(), Error> {
        let c = cycle.clone();
        let changes = changes.to_vec();

        with_conn(&self.conn, move |conn| {
            let tx = conn
                .unchecked_transaction()
                .map_err(|e| Error::Storage(e.to_string()))?;

            tx.execute(
                "INSERT INTO dream_cycles
                    (id, agent_id, kind, status, started_at, promoted_at, summary,
                     rustykrab_version)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    c.id.to_string(),
                    c.agent_id.to_string(),
                    c.kind,
                    c.status.as_str(),
                    c.started_at.to_rfc3339(),
                    c.promoted_at.map(|t| t.to_rfc3339()),
                    c.summary,
                    c.rustykrab_version,
                ],
            )
            .map_err(|e| Error::Storage(e.to_string()))?;

            {
                let mut stmt = tx
                    .prepare(
                        "INSERT INTO dream_changes (cycle_id, seq, op, target_id, payload)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                    )
                    .map_err(|e| Error::Storage(e.to_string()))?;
                for (seq, change) in changes.iter().enumerate() {
                    let payload = serde_json::to_string(change)
                        .map_err(|e| Error::Storage(format!("cannot encode change: {e}")))?;
                    stmt.execute(params![
                        c.id.to_string(),
                        seq as i64,
                        change.op_name(),
                        change.target_id().to_string(),
                        payload,
                    ])
                    .map_err(|e| Error::Storage(e.to_string()))?;
                }
            }

            tx.commit().map_err(|e| Error::Storage(e.to_string()))?;
            Ok(())
        })
        .await
    }

    /// Move a cycle to a new status, stamping `promoted_at` on the way live.
    pub async fn set_status(&self, cycle_id: Uuid, status: CycleStatus) -> Result<(), Error> {
        let id = cycle_id.to_string();
        let promoted_at = status.is_live().then(|| Utc::now().to_rfc3339());

        with_conn(&self.conn, move |conn| {
            let changed = conn
                .execute(
                    "UPDATE dream_cycles
                     SET status = ?2,
                         promoted_at = COALESCE(?3, promoted_at)
                     WHERE id = ?1",
                    params![id, status.as_str(), promoted_at],
                )
                .map_err(|e| Error::Storage(e.to_string()))?;
            if changed == 0 {
                return Err(Error::NotFound(format!("dream cycle {id}")));
            }
            Ok(())
        })
        .await
    }

    /// Attach a human-readable account of what a cycle did.
    pub async fn set_summary(&self, cycle_id: Uuid, summary: &str) -> Result<(), Error> {
        let id = cycle_id.to_string();
        let summary = summary.to_string();
        with_conn(&self.conn, move |conn| {
            conn.execute(
                "UPDATE dream_cycles SET summary = ?2 WHERE id = ?1",
                params![id, summary],
            )
            .map_err(|e| Error::Storage(e.to_string()))?;
            Ok(())
        })
        .await
    }

    pub async fn get(&self, cycle_id: Uuid) -> Result<Option<DreamCycle>, Error> {
        let id = cycle_id.to_string();
        with_conn(&self.conn, move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, agent_id, kind, status, started_at, promoted_at, summary,
                            rustykrab_version
                     FROM dream_cycles WHERE id = ?1",
                )
                .map_err(|e| Error::Storage(e.to_string()))?;
            let mut rows = stmt
                .query_map(params![id], row_to_cycle)
                .map_err(|e| Error::Storage(e.to_string()))?;
            match rows.next() {
                Some(row) => Ok(Some(row.map_err(|e| Error::Storage(e.to_string()))?)),
                None => Ok(None),
            }
        })
        .await
    }

    /// A cycle's change-set, in the order it was planned.
    pub async fn changes(&self, cycle_id: Uuid) -> Result<Vec<StagedChange>, Error> {
        let id = cycle_id.to_string();
        with_conn(&self.conn, move |conn| {
            let mut stmt = conn
                .prepare("SELECT payload FROM dream_changes WHERE cycle_id = ?1 ORDER BY seq ASC")
                .map_err(|e| Error::Storage(e.to_string()))?;
            let rows = stmt
                .query_map(params![id], |row| row.get::<_, String>(0))
                .map_err(|e| Error::Storage(e.to_string()))?;

            let mut out = Vec::new();
            for row in rows {
                let payload = row.map_err(|e| Error::Storage(e.to_string()))?;
                let change: StagedChange = serde_json::from_str(&payload)
                    .map_err(|e| Error::Storage(format!("cannot decode change: {e}")))?;
                out.push(change);
            }
            Ok(out)
        })
        .await
    }

    /// The changes promotion actually applied, in planned order.
    ///
    /// This, not [`Self::changes`], is what a reversal must walk.
    /// Promotion skips changes whose target moved between planning and
    /// promoting; reversing one of those restores a memory this cycle
    /// never retired, undoing whatever decision did retire it. The staged
    /// set is what the cycle intended, which is a different question and
    /// useful only for audit.
    pub async fn applied_changes(&self, cycle_id: Uuid) -> Result<Vec<StagedChange>, Error> {
        let id = cycle_id.to_string();
        with_conn(&self.conn, move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT payload FROM dream_changes
                     WHERE cycle_id = ?1 AND applied = 1
                     ORDER BY seq ASC",
                )
                .map_err(|e| Error::Storage(e.to_string()))?;
            let rows = stmt
                .query_map(params![id], |row| row.get::<_, String>(0))
                .map_err(|e| Error::Storage(e.to_string()))?;

            let mut out = Vec::new();
            for row in rows {
                let payload = row.map_err(|e| Error::Storage(e.to_string()))?;
                let change: StagedChange = serde_json::from_str(&payload)
                    .map_err(|e| Error::Storage(format!("cannot decode change: {e}")))?;
                out.push(change);
            }
            Ok(out)
        })
        .await
    }

    /// Record which of a cycle's staged changes were applied.
    ///
    /// Matched by payload, because that is what uniquely identifies a
    /// staged row: a cycle can stage two changes against the same target
    /// only if they differ, and identical payloads are interchangeable for
    /// reversal purposes.
    pub async fn mark_applied(
        &self,
        cycle_id: Uuid,
        applied: &[StagedChange],
    ) -> Result<usize, Error> {
        if applied.is_empty() {
            return Ok(0);
        }
        let id = cycle_id.to_string();
        let payloads = applied
            .iter()
            .map(|c| {
                serde_json::to_string(c)
                    .map_err(|e| Error::Storage(format!("cannot encode change: {e}")))
            })
            .collect::<Result<Vec<_>, Error>>()?;

        with_conn(&self.conn, move |conn| {
            let tx = conn
                .unchecked_transaction()
                .map_err(|e| Error::Storage(e.to_string()))?;
            let mut marked = 0usize;
            {
                let mut stmt = tx
                    .prepare(
                        "UPDATE dream_changes SET applied = 1
                         WHERE cycle_id = ?1 AND payload = ?2",
                    )
                    .map_err(|e| Error::Storage(e.to_string()))?;
                for payload in &payloads {
                    marked += stmt
                        .execute(params![id, payload])
                        .map_err(|e| Error::Storage(e.to_string()))?;
                }
            }
            tx.commit().map_err(|e| Error::Storage(e.to_string()))?;
            Ok(marked)
        })
        .await
    }

    /// Cycles in a given status, newest first.
    pub async fn list_by_status(
        &self,
        agent_id: Uuid,
        status: CycleStatus,
        limit: u32,
    ) -> Result<Vec<DreamCycle>, Error> {
        let agent = agent_id.to_string();
        with_conn(&self.conn, move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, agent_id, kind, status, started_at, promoted_at, summary,
                            rustykrab_version
                     FROM dream_cycles
                     WHERE agent_id = ?1 AND status = ?2
                     ORDER BY started_at DESC
                     LIMIT ?3",
                )
                .map_err(|e| Error::Storage(e.to_string()))?;
            let rows = stmt
                .query_map(params![agent, status.as_str(), limit], row_to_cycle)
                .map_err(|e| Error::Storage(e.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| Error::Storage(e.to_string()))?;
            Ok(rows)
        })
        .await
    }

    /// The most recent promoted cycle, which is the one a rollback would
    /// target.
    pub async fn latest_promoted(&self, agent_id: Uuid) -> Result<Option<DreamCycle>, Error> {
        Ok(self
            .list_by_status(agent_id, CycleStatus::Promoted, 1)
            .await?
            .into_iter()
            .next())
    }
}

fn row_to_cycle(row: &rusqlite::Row<'_>) -> rusqlite::Result<DreamCycle> {
    let parse_uuid = |s: String| Uuid::parse_str(&s).unwrap_or_default();
    let started_at: String = row.get("started_at")?;
    let promoted_at: Option<String> = row.get("promoted_at")?;

    Ok(DreamCycle {
        id: parse_uuid(row.get("id")?),
        agent_id: parse_uuid(row.get("agent_id")?),
        kind: row.get("kind")?,
        status: CycleStatus::parse(&row.get::<_, String>("status")?)
            .unwrap_or(CycleStatus::Aborted),
        started_at: DateTime::parse_from_rfc3339(&started_at)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now()),
        promoted_at: promoted_at.and_then(|s| {
            DateTime::parse_from_rfc3339(&s)
                .ok()
                .map(|dt| dt.with_timezone(&Utc))
        }),
        summary: row.get("summary")?,
        rustykrab_version: row.get("rustykrab_version")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> DreamCycleStore {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        crate::Store::run_migrations(&conn).unwrap();
        DreamCycleStore::new(Arc::new(Mutex::new(conn)))
    }

    fn consolidation(parents: &[Uuid], child: Uuid) -> Vec<StagedChange> {
        let mut changes = vec![StagedChange::CreateMemory {
            memory_id: child,
            content: "merged".into(),
            parent_ids: parents.to_vec(),
        }];
        for p in parents {
            changes.push(StagedChange::InvalidateMemory {
                memory_id: *p,
                superseded_by: child,
                expected_content_hash: format!("hash-{p}"),
            });
        }
        changes
    }

    #[tokio::test]
    async fn a_staged_cycle_is_recorded_but_not_live() {
        let store = test_store();
        let agent = Uuid::new_v4();
        let cycle = DreamCycle::new(agent, "memory_consolidation");
        let changes = consolidation(&[Uuid::new_v4()], Uuid::new_v4());

        store.stage(&cycle, &changes).await.unwrap();

        let stored = store.get(cycle.id).await.unwrap().unwrap();
        assert_eq!(stored.status, CycleStatus::Staged);
        assert!(
            !stored.status.is_live(),
            "staging must not mark a cycle live"
        );
        assert!(
            stored.promoted_at.is_none(),
            "nothing was promoted, so there is no promotion time"
        );
    }

    #[tokio::test]
    async fn change_set_round_trips_in_planned_order() {
        // Order is not cosmetic: reversal walks it backwards, so a
        // scrambled read would undo a cycle in the wrong sequence.
        let store = test_store();
        let child = Uuid::new_v4();
        let parents = vec![Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
        let cycle = DreamCycle::new(Uuid::new_v4(), "memory_consolidation");
        let changes = consolidation(&parents, child);

        store.stage(&cycle, &changes).await.unwrap();
        let read = store.changes(cycle.id).await.unwrap();

        assert_eq!(read, changes, "change-set must survive verbatim, in order");
        assert!(read[0].is_additive(), "the create is planned first");
    }

    #[tokio::test]
    async fn promoting_stamps_the_time_it_went_live() {
        let store = test_store();
        let cycle = DreamCycle::new(Uuid::new_v4(), "memory_consolidation");
        store.stage(&cycle, &[]).await.unwrap();

        store
            .set_status(cycle.id, CycleStatus::Promoted)
            .await
            .unwrap();

        let stored = store.get(cycle.id).await.unwrap().unwrap();
        assert_eq!(stored.status, CycleStatus::Promoted);
        assert!(stored.status.is_live());
        assert!(stored.promoted_at.is_some());
    }

    #[tokio::test]
    async fn rollback_preserves_when_the_cycle_had_been_live() {
        // The promotion time is evidence about the past. Clearing it on
        // rollback would erase the fact that live state once reflected
        // this cycle, which is exactly what an audit needs.
        let store = test_store();
        let cycle = DreamCycle::new(Uuid::new_v4(), "memory_consolidation");
        store.stage(&cycle, &[]).await.unwrap();
        store
            .set_status(cycle.id, CycleStatus::Promoted)
            .await
            .unwrap();
        let promoted_at = store.get(cycle.id).await.unwrap().unwrap().promoted_at;

        store
            .set_status(cycle.id, CycleStatus::RolledBack)
            .await
            .unwrap();

        let stored = store.get(cycle.id).await.unwrap().unwrap();
        assert_eq!(stored.status, CycleStatus::RolledBack);
        assert!(!stored.status.is_live());
        assert_eq!(
            stored.promoted_at, promoted_at,
            "a rolled-back cycle must still record that it was once live"
        );
    }

    #[tokio::test]
    async fn latest_promoted_ignores_staged_and_aborted_cycles() {
        let store = test_store();
        let agent = Uuid::new_v4();

        let aborted = DreamCycle::new(agent, "memory_consolidation");
        store.stage(&aborted, &[]).await.unwrap();
        store
            .set_status(aborted.id, CycleStatus::Aborted)
            .await
            .unwrap();

        let staged = DreamCycle::new(agent, "memory_consolidation");
        store.stage(&staged, &[]).await.unwrap();

        let live = DreamCycle::new(agent, "memory_consolidation");
        store.stage(&live, &[]).await.unwrap();
        store
            .set_status(live.id, CycleStatus::Promoted)
            .await
            .unwrap();

        let found = store.latest_promoted(agent).await.unwrap().unwrap();
        assert_eq!(
            found.id, live.id,
            "rollback must target the promoted cycle, not a staged or aborted one"
        );
    }

    #[tokio::test]
    async fn cycles_are_scoped_per_agent() {
        let store = test_store();
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());

        let theirs = DreamCycle::new(b, "memory_consolidation");
        store.stage(&theirs, &[]).await.unwrap();
        store
            .set_status(theirs.id, CycleStatus::Promoted)
            .await
            .unwrap();

        assert!(
            store.latest_promoted(a).await.unwrap().is_none(),
            "one agent's cycle must never be a rollback target for another"
        );
    }

    #[tokio::test]
    async fn changes_are_removed_with_their_cycle() {
        let store = test_store();
        let cycle = DreamCycle::new(Uuid::new_v4(), "memory_consolidation");
        store
            .stage(&cycle, &consolidation(&[Uuid::new_v4()], Uuid::new_v4()))
            .await
            .unwrap();
        assert_eq!(store.changes(cycle.id).await.unwrap().len(), 2);

        with_conn(&store.conn, move |conn| {
            conn.execute("DELETE FROM dream_cycles", [])
                .map_err(|e| Error::Storage(e.to_string()))?;
            Ok(())
        })
        .await
        .unwrap();

        assert!(
            store.changes(cycle.id).await.unwrap().is_empty(),
            "orphaned changes would describe a cycle that no longer exists"
        );
    }

    #[tokio::test]
    async fn setting_status_on_an_unknown_cycle_is_an_error() {
        // Silently succeeding here would let a promote report success
        // while nothing was recorded as live.
        let store = test_store();
        let result = store
            .set_status(Uuid::new_v4(), CycleStatus::Promoted)
            .await;
        assert!(result.is_err());
    }
}
