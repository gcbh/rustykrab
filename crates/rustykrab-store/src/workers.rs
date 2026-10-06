//! The worker registry's rows (`docs/plans/control-layer-and-worker-fleet.md`,
//! sections 5, 10 and 13).
//!
//! One row per named worker: its kind, what it advertises, how to rebuild
//! it after a restart, its health and cost tier, and its routing record,
//! the per-class tally the controller writes from verified results and
//! reads when it matches work to workers. The registry and the routing
//! policy live in `rustykrab-control`; this module stores their rows and
//! nothing else.
//!
//! The routing record is JSON keyed by work class, one [`ClassRecord`]
//! each. Dreaming (Phase 6) reads it from here, so its shape is part of
//! this crate's contract and is written out in `ARCHITECTURE.md`. Every
//! change to a record goes through [`WorkerStore::update_record`], a
//! read-modify-write inside one transaction, so two results judged at
//! once never lose an update.
//!
//! Beside it, `routing_defaults` holds each routed class's default tier:
//! the lowest cost tier that takes the class without an earned record. The
//! controller seeds it with its policy's prior and reads it; it never moves
//! one. Moving a class's default is what an accepted routing proposal does
//! (Phase 6), through [`WorkerStore::set_default_tier`].
//!
//! Parsing is conservative, as for work items: a JSON column that does not
//! parse reads as empty (no capabilities, no configuration, no record), so
//! an unreadable row never earns a worker work it has not shown it can do.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{params, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

use rustykrab_core::work::WorkerKind;
use rustykrab_core::Error;

use crate::with_conn;

/// Create the `workers` table. Called from `Store::run_migrations`.
pub(crate) fn migrate(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(
        "
        -- Named workers (control-layer plan, sections 5 and 13). `name` is
        -- the registry's name, the one leases, events and notices carry;
        -- nothing references it with a foreign key, because a worker's
        -- history must outlive its removal. `capabilities`, `config` and
        -- `routing_record` are JSON; an unreadable one reads as empty.
        CREATE TABLE IF NOT EXISTS workers (
            name           TEXT PRIMARY KEY,
            kind           TEXT NOT NULL,
            capabilities   TEXT NOT NULL DEFAULT '{}',
            config         TEXT NOT NULL DEFAULT '{}',
            health         TEXT NOT NULL DEFAULT 'unknown',
            last_seen      TEXT,
            cost_tier      INTEGER NOT NULL DEFAULT 0,
            routing_record TEXT NOT NULL DEFAULT '{}',
            created_at     TEXT NOT NULL,
            updated_at     TEXT NOT NULL
        );

        -- Each routed class's default tier (plan section 10). `set_by` is
        -- `policy` for the controller's seeded prior, else what moved it
        -- (an accepted routing proposal's item id).
        CREATE TABLE IF NOT EXISTS routing_defaults (
            class  TEXT PRIMARY KEY,
            tier   INTEGER NOT NULL,
            set_by TEXT NOT NULL,
            reason TEXT,
            set_at TEXT NOT NULL
        );
        ",
    )
    .map_err(|e| Error::Storage(e.to_string()))
}

/// What one class of work cost a worker, summed over its judged runs.
/// `tokens` stays 0 for a worker that does not report them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassCost {
    pub runs: u64,
    pub wall_seconds: u64,
    #[serde(default)]
    pub tokens: u64,
}

/// One worker's record for one class of work (plan section 5's
/// `routing_record` entry, section 10's "coding quality by worker").
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassRecord {
    /// Results the controller verified from evidence and closed `done`.
    pub verified_done: u64,
    /// Results that claimed done and failed verification.
    pub claimed_not_verified: u64,
    /// Runs that ended in an error the worker reported or caused.
    #[serde(default)]
    pub failed: u64,
    /// Defects found after a verified result was accepted. Nothing writes
    /// it yet; the field is part of the shape dreaming reads.
    #[serde(default)]
    pub escaped_defects: u64,
    /// Repair rungs the items it finished took before they verified.
    #[serde(default)]
    pub repairs: u64,
    #[serde(default)]
    pub cost: ClassCost,
    /// Set while the worker has not yet earned the class, or its last
    /// result on it failed verification.
    #[serde(default = "default_probation")]
    pub probation: bool,
    #[serde(default)]
    pub last_verified_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_failed_at: Option<DateTime<Utc>>,
    /// The latest items judged on this class, oldest first, at most
    /// [`RECENT_ITEMS`]: the results a routing proposal points a reviewer
    /// at.
    #[serde(default)]
    pub recent_items: Vec<String>,
}

/// How many item ids a class record keeps.
pub const RECENT_ITEMS: usize = 10;

impl ClassRecord {
    /// Note `item` as the latest judged, once, keeping at most
    /// [`RECENT_ITEMS`].
    pub fn note_item(&mut self, item: &str) {
        self.recent_items.retain(|i| i != item);
        self.recent_items.push(item.to_string());
        let over = self.recent_items.len().saturating_sub(RECENT_ITEMS);
        self.recent_items.drain(..over);
    }
}

fn default_probation() -> bool {
    true
}

/// A worker's routing record: one [`ClassRecord`] per work class, keyed
/// by the class name (`code`, `capability:build`, ...).
pub type RoutingRecord = BTreeMap<String, ClassRecord>;

/// One `workers` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerRow {
    pub name: String,
    /// As stored; [`WorkerRow::kind`] reads it.
    pub kind: String,
    pub capabilities: serde_json::Value,
    /// What the registry needs to rebuild the worker after a restart (an
    /// external adapter's command, repositories and limits). Empty for a
    /// worker the daemon builds itself.
    pub config: serde_json::Value,
    pub health: String,
    pub last_seen: Option<DateTime<Utc>>,
    pub cost_tier: u32,
    pub routing_record: RoutingRecord,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl WorkerRow {
    /// The worker kind, or `None` for a kind this build does not know.
    pub fn kind(&self) -> Option<WorkerKind> {
        WorkerKind::parse(&self.kind)
    }

    fn from_row(row: &Row) -> rusqlite::Result<WorkerRow> {
        let json = |column: &str| -> rusqlite::Result<serde_json::Value> {
            let raw: String = row.get(column)?;
            Ok(serde_json::from_str(&raw).unwrap_or_else(|e| {
                tracing::warn!(column, error = %e, "unreadable worker column; read as empty");
                serde_json::Value::Object(serde_json::Map::new())
            }))
        };
        let record: String = row.get("routing_record")?;
        let routing_record = serde_json::from_str(&record).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "unreadable routing record; read as empty");
            RoutingRecord::new()
        });
        let tier: i64 = row.get("cost_tier")?;
        Ok(WorkerRow {
            name: row.get("name")?,
            kind: row.get("kind")?,
            capabilities: json("capabilities")?,
            config: json("config")?,
            health: row.get("health")?,
            last_seen: row
                .get::<_, Option<String>>("last_seen")?
                .as_deref()
                .and_then(parse_ts),
            cost_tier: u32::try_from(tier).unwrap_or(u32::MAX),
            routing_record,
            created_at: parse_ts(&row.get::<_, String>("created_at")?).unwrap_or_default(),
            updated_at: parse_ts(&row.get::<_, String>("updated_at")?).unwrap_or_default(),
        })
    }
}

/// One class's default tier: the lowest cost tier that takes the class
/// without an earned record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingDefault {
    pub class: String,
    pub tier: u32,
    /// `policy` for the controller's prior, else what moved it.
    pub set_by: String,
    pub reason: Option<String>,
    pub set_at: DateTime<Utc>,
}

/// What a registration writes. The routing record is never written here:
/// it belongs to [`WorkerStore::update_record`].
#[derive(Debug, Clone, PartialEq)]
pub struct WorkerUpsert {
    pub name: String,
    pub kind: WorkerKind,
    pub capabilities: serde_json::Value,
    pub config: serde_json::Value,
    pub health: String,
    pub last_seen: Option<DateTime<Utc>>,
    pub cost_tier: u32,
}

fn ts(t: &DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn parse_ts(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

fn storage(e: impl std::fmt::Display) -> Error {
    Error::Storage(e.to_string())
}

const COLUMNS: &str = "name, kind, capabilities, config, health, last_seen, cost_tier, \
     routing_record, created_at, updated_at";

/// Handle for the `workers` table.
#[derive(Clone)]
pub struct WorkerStore {
    conn: Arc<Mutex<rusqlite::Connection>>,
}

impl WorkerStore {
    pub(crate) fn new(conn: Arc<Mutex<rusqlite::Connection>>) -> Self {
        Self { conn }
    }

    /// Insert a worker, or update everything but its routing record and
    /// creation time when the name is taken.
    pub async fn upsert(&self, row: WorkerUpsert) -> Result<WorkerRow, Error> {
        with_conn(&self.conn, move |conn| {
            let now = ts(&Utc::now());
            conn.execute(
                "INSERT INTO workers (name, kind, capabilities, config, health, last_seen,
                                      cost_tier, routing_record, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, '{}', ?8, ?8)
                 ON CONFLICT(name) DO UPDATE SET
                     kind = excluded.kind,
                     capabilities = excluded.capabilities,
                     config = excluded.config,
                     health = excluded.health,
                     last_seen = COALESCE(excluded.last_seen, workers.last_seen),
                     cost_tier = excluded.cost_tier,
                     updated_at = excluded.updated_at",
                params![
                    row.name,
                    row.kind.as_str(),
                    row.capabilities.to_string(),
                    row.config.to_string(),
                    row.health,
                    row.last_seen.as_ref().map(ts),
                    i64::from(row.cost_tier),
                    now,
                ],
            )
            .map_err(storage)?;
            get(conn, &row.name)?
                .ok_or_else(|| Error::Storage(format!("worker {} vanished", row.name)))
        })
        .await
    }

    pub async fn get(&self, name: &str) -> Result<Option<WorkerRow>, Error> {
        let name = name.to_string();
        with_conn(&self.conn, move |conn| get(conn, &name)).await
    }

    /// Every worker, oldest registration first.
    pub async fn list(&self) -> Result<Vec<WorkerRow>, Error> {
        with_conn(&self.conn, |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {COLUMNS} FROM workers ORDER BY created_at, name"
                ))
                .map_err(storage)?;
            let rows = stmt
                .query_map([], WorkerRow::from_row)
                .map_err(storage)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(storage)?;
            Ok(rows)
        })
        .await
    }

    /// Remove a worker's row. Its leases, events and evidence keep naming
    /// it: they are history. Returns whether a row was removed.
    pub async fn remove(&self, name: &str) -> Result<bool, Error> {
        let name = name.to_string();
        with_conn(&self.conn, move |conn| {
            let n = conn
                .execute("DELETE FROM workers WHERE name = ?1", params![name])
                .map_err(storage)?;
            Ok(n > 0)
        })
        .await
    }

    /// Record a health check: the health line, and `last_seen` when the
    /// worker answered.
    pub async fn touch(
        &self,
        name: &str,
        health: &str,
        seen: Option<DateTime<Utc>>,
    ) -> Result<(), Error> {
        let (name, health) = (name.to_string(), health.to_string());
        with_conn(&self.conn, move |conn| {
            conn.execute(
                "UPDATE workers
                    SET health = ?2, last_seen = COALESCE(?3, last_seen), updated_at = ?4
                  WHERE name = ?1",
                params![name, health, seen.as_ref().map(ts), ts(&Utc::now())],
            )
            .map_err(storage)?;
            Ok(())
        })
        .await
    }

    /// Record what a worker advertises now (a peer node's models, tools,
    /// MCP servers and machine, plan section 5) with the health check that
    /// read it: the capabilities are replaced only when the worker
    /// answered (`capabilities` is `Some`), so a node that is down keeps
    /// the last advertisement it gave, marked by its health line.
    pub async fn advertise(
        &self,
        name: &str,
        capabilities: Option<serde_json::Value>,
        health: &str,
        seen: Option<DateTime<Utc>>,
    ) -> Result<(), Error> {
        let (name, health) = (name.to_string(), health.to_string());
        with_conn(&self.conn, move |conn| {
            conn.execute(
                "UPDATE workers
                    SET capabilities = COALESCE(?2, capabilities), health = ?3,
                        last_seen = COALESCE(?4, last_seen), updated_at = ?5
                  WHERE name = ?1",
                params![
                    name,
                    capabilities.map(|c| c.to_string()),
                    health,
                    seen.as_ref().map(ts),
                    ts(&Utc::now())
                ],
            )
            .map_err(storage)?;
            Ok(())
        })
        .await
    }

    /// Change a worker's routing record in one transaction: read it, let
    /// `change` edit it, write it back. Returns the record as written, or
    /// `NotFound` when no worker has the name.
    pub async fn update_record<F>(&self, name: &str, change: F) -> Result<RoutingRecord, Error>
    where
        F: FnOnce(&mut RoutingRecord) + Send + 'static,
    {
        let name = name.to_string();
        with_conn(&self.conn, move |conn| {
            let tx = conn.unchecked_transaction().map_err(storage)?;
            let raw: Option<String> = tx
                .query_row(
                    "SELECT routing_record FROM workers WHERE name = ?1",
                    params![name],
                    |r| r.get(0),
                )
                .optional()
                .map_err(storage)?;
            let Some(raw) = raw else {
                return Err(Error::NotFound(format!("worker {name}")));
            };
            let mut record: RoutingRecord = serde_json::from_str(&raw).unwrap_or_default();
            change(&mut record);
            let json = serde_json::to_string(&record).map_err(storage)?;
            tx.execute(
                "UPDATE workers SET routing_record = ?2, updated_at = ?3 WHERE name = ?1",
                params![name, json, ts(&Utc::now())],
            )
            .map_err(storage)?;
            tx.commit().map_err(storage)?;
            Ok(record)
        })
        .await
    }
}

impl WorkerStore {
    /// Every class's default tier.
    pub async fn default_tiers(&self) -> Result<Vec<RoutingDefault>, Error> {
        with_conn(&self.conn, |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT class, tier, set_by, reason, set_at
                       FROM routing_defaults ORDER BY class",
                )
                .map_err(storage)?;
            let rows = stmt
                .query_map([], |row| {
                    let tier: i64 = row.get(1)?;
                    Ok(RoutingDefault {
                        class: row.get(0)?,
                        tier: u32::try_from(tier).unwrap_or(u32::MAX),
                        set_by: row.get(2)?,
                        reason: row.get(3)?,
                        set_at: parse_ts(&row.get::<_, String>(4)?).unwrap_or_default(),
                    })
                })
                .map_err(storage)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(storage)?;
            Ok(rows)
        })
        .await
    }

    /// Record a class's default tier unless one is recorded already: the
    /// controller's policy prior, seeded at start. Returns whether it wrote.
    pub async fn seed_default_tier(&self, class: &str, tier: u32) -> Result<bool, Error> {
        let class = class.to_string();
        with_conn(&self.conn, move |conn| {
            let n = conn
                .execute(
                    "INSERT OR IGNORE INTO routing_defaults (class, tier, set_by, reason, set_at)
                     VALUES (?1, ?2, 'policy', NULL, ?3)",
                    params![class, i64::from(tier), ts(&Utc::now())],
                )
                .map_err(storage)?;
            Ok(n > 0)
        })
        .await
    }

    /// Move a class's default tier: what an accepted routing proposal does
    /// (plan section 10). The controller never calls it.
    pub async fn set_default_tier(
        &self,
        class: &str,
        tier: u32,
        set_by: &str,
        reason: Option<&str>,
    ) -> Result<(), Error> {
        let (class, set_by) = (class.to_string(), set_by.to_string());
        let reason = reason.map(str::to_string);
        with_conn(&self.conn, move |conn| {
            conn.execute(
                "INSERT INTO routing_defaults (class, tier, set_by, reason, set_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(class) DO UPDATE SET
                     tier = excluded.tier, set_by = excluded.set_by,
                     reason = excluded.reason, set_at = excluded.set_at",
                params![class, i64::from(tier), set_by, reason, ts(&Utc::now())],
            )
            .map_err(storage)?;
            Ok(())
        })
        .await
    }
}

fn get(conn: &rusqlite::Connection, name: &str) -> Result<Option<WorkerRow>, Error> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM workers WHERE name = ?1"),
        params![name],
        WorkerRow::from_row,
    )
    .optional()
    .map_err(storage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;
    use serde_json::json;

    fn store() -> Store {
        Store::open_in_memory()
    }

    fn upsert(name: &str, kind: WorkerKind) -> WorkerUpsert {
        WorkerUpsert {
            name: name.to_string(),
            kind,
            capabilities: json!({ "repos": ["/tmp/repo"] }),
            config: json!({ "command": "claude" }),
            health: "healthy".to_string(),
            last_seen: Some(Utc::now()),
            cost_tier: 3,
        }
    }

    #[tokio::test]
    async fn a_worker_round_trips_and_a_second_upsert_keeps_its_record() {
        let store = store();
        let workers = store.workers();
        let row = workers
            .upsert(upsert("pinch", WorkerKind::ClaudeCode))
            .await
            .unwrap();
        assert_eq!(row.kind(), Some(WorkerKind::ClaudeCode));
        assert_eq!(row.capabilities["repos"][0], "/tmp/repo");
        assert!(row.routing_record.is_empty());

        workers
            .update_record("pinch", |r| {
                let class = r.entry("code".to_string()).or_default();
                class.verified_done += 1;
                class.cost.runs += 1;
            })
            .await
            .unwrap();
        let mut again = upsert("pinch", WorkerKind::ClaudeCode);
        again.health = "unhealthy: command missing".to_string();
        again.last_seen = None;
        let row = workers.upsert(again).await.unwrap();
        assert_eq!(row.routing_record["code"].verified_done, 1);
        assert_eq!(row.health, "unhealthy: command missing");
        assert!(row.last_seen.is_some(), "a missing last_seen keeps the old");

        workers
            .upsert(upsert("krabby", WorkerKind::Local))
            .await
            .unwrap();
        let names: Vec<String> = workers
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|w| w.name)
            .collect();
        assert_eq!(names, ["pinch", "krabby"]);
        assert!(workers.remove("pinch").await.unwrap());
        assert!(!workers.remove("pinch").await.unwrap());
        assert!(workers.get("pinch").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn record_updates_do_not_lose_each_other() {
        let store = store();
        let workers = store.workers();
        workers
            .upsert(upsert("pinch", WorkerKind::ClaudeCode))
            .await
            .unwrap();
        let mut tasks = Vec::new();
        for _ in 0..20 {
            let w = workers.clone();
            tasks.push(tokio::spawn(async move {
                w.update_record("pinch", |r| {
                    r.entry("code".to_string()).or_default().verified_done += 1;
                })
                .await
                .unwrap();
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        let row = workers.get("pinch").await.unwrap().unwrap();
        assert_eq!(row.routing_record["code"].verified_done, 20);
        let missing = workers.update_record("nobody", |_| {}).await;
        assert!(matches!(missing, Err(Error::NotFound(_))));
    }

    #[tokio::test]
    async fn a_seeded_default_tier_stays_until_something_moves_it() {
        let store = store();
        let workers = store.workers();
        assert!(workers.seed_default_tier("code", 0).await.unwrap());
        assert!(
            !workers.seed_default_tier("code", 2).await.unwrap(),
            "seeded once"
        );
        workers
            .set_default_tier("code", 3, "item-9", Some("the local record fails"))
            .await
            .unwrap();
        assert!(!workers.seed_default_tier("code", 0).await.unwrap());
        let tiers = workers.default_tiers().await.unwrap();
        assert_eq!(tiers.len(), 1);
        assert_eq!((tiers[0].tier, tiers[0].set_by.as_str()), (3, "item-9"));
    }

    #[tokio::test]
    async fn unreadable_json_reads_as_empty_and_an_old_record_gains_defaults() {
        let store = store();
        let workers = store.workers();
        workers
            .upsert(upsert("pinch", WorkerKind::Codex))
            .await
            .unwrap();
        let conn = store.conn.clone();
        with_conn(&conn, |c| {
            c.execute(
                "UPDATE workers SET capabilities = 'not json',
                        routing_record = '{\"code\":{\"verified_done\":2,\"claimed_not_verified\":1}}'",
                [],
            )
            .map_err(storage)?;
            Ok(())
        })
        .await
        .unwrap();
        let row = workers.get("pinch").await.unwrap().unwrap();
        assert_eq!(row.capabilities, json!({}));
        let code = &row.routing_record["code"];
        assert_eq!(code.verified_done, 2);
        assert_eq!(code.claimed_not_verified, 1);
        assert!(code.probation, "an old record reads as on probation");
        assert_eq!(code.cost, ClassCost::default());
    }
}
