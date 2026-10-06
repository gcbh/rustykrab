//! Column encodings and row mappers for the work-item tables.
//!
//! Timestamps are RFC 3339 in UTC with nanoseconds, always the same width,
//! so text comparison in SQL orders them and a round trip is exact.
//!
//! Parsing is conservative (section 13). A work item with any column the
//! mapper cannot interpret (an unknown status, kind or worker kind, JSON that
//! does not parse, a time that does not parse, a priority out of range)
//! reads as `failed`, so it never becomes work that runs early; the other
//! fields fall back to their defaults so the row stays displayable. Every
//! such row is logged. Unknown edge kinds read as `blocks`, through
//! `EdgeKind::parse`.

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::Row;
use serde::de::DeserializeOwned;
use serde::Serialize;

use rustykrab_core::work::{
    Edge, EdgeKind, EventKind, Evidence, Lease, Status, WorkEvent, WorkItem, WorkKind, WorkerKind,
};

use super::{ArchivedItem, OutboxRow, WorkPlanRow, WorkStoreError};

pub(super) fn ts(t: &DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

pub(super) fn parse_ts(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

pub(super) fn to_json<T: Serialize>(value: &T) -> Result<String, WorkStoreError> {
    Ok(serde_json::to_string(value)?)
}

/// Column list for `SELECT`s and the `INSERT` against `work_items`. The
/// insert binds its parameters in this order.
pub(super) const ITEM_COLUMNS: &str = "id, kind, title, objective, done_when, status, \
     status_reason, status_origin, priority, parent, worker_kind, origin_conversation_id, \
     trigger_at, expires_at, plan_id, held_by, created_at, updated_at, closed_at, \
     constraints, decisions_made, artifact_refs, required_tools, required_mcp_servers, \
     writable_resources, inputs_from, preconditions, budget, trigger";

pub(super) const EVENT_COLUMNS: &str = "item, at, kind, from_status, from_reason, to_status, \
     to_reason, actor, reason, upstream, origin, evidence_ref";

pub(super) const EVIDENCE_COLUMNS: &str = "item, kind, ref, hash, verified_by, at";

pub(super) const LEASE_COLUMNS: &str = "item, worker, since, ttl_seconds, heartbeat_at, inputs";

pub(super) const PLAN_COLUMNS: &str =
    "id, root, filed_by, rationale, approval_question, policy, created_at";

pub(super) const OUTBOX_COLUMNS: &str =
    "id, parent, origin, channel, body, created_at, delivered_at";

pub(super) const ARCHIVE_COLUMNS: &str = "id, kind, title, parent, status, status_reason, \
     worker, cost, closed_at, archived_at, summary, edges";

/// The columns of one row that did not parse.
struct Unreadable(Vec<&'static str>);

impl Unreadable {
    fn json<T: DeserializeOwned + Default>(&mut self, column: &'static str, raw: &str) -> T {
        serde_json::from_str(raw).unwrap_or_else(|_| {
            self.0.push(column);
            T::default()
        })
    }

    fn time(&mut self, column: &'static str, raw: Option<String>) -> Option<DateTime<Utc>> {
        let raw = raw?;
        let parsed = parse_ts(&raw);
        if parsed.is_none() {
            self.0.push(column);
        }
        parsed
    }

    fn required_time(&mut self, column: &'static str, raw: String) -> DateTime<Utc> {
        self.time(column, Some(raw)).unwrap_or_default()
    }
}

/// Map a `SELECT {ITEM_COLUMNS}` row. See the module doc for what an
/// unreadable column does.
pub(super) fn item_from_row(row: &Row) -> rusqlite::Result<WorkItem> {
    let mut bad = Unreadable(Vec::new());

    let kind_raw: String = row.get("kind")?;
    let kind = WorkKind::parse(&kind_raw).unwrap_or_else(|| {
        bad.0.push("kind");
        WorkKind::Internal
    });
    let worker_kind_raw: String = row.get("worker_kind")?;
    let worker_kind = WorkerKind::parse(&worker_kind_raw).unwrap_or_else(|| {
        bad.0.push("worker_kind");
        WorkerKind::Any
    });

    let status_raw: String = row.get("status")?;
    let status_reason: Option<String> = row.get("status_reason")?;
    let status = Status::parse(&status_raw, status_reason.as_deref());
    if status == Status::Failed && status_raw != "failed" {
        // `Status::parse` fell back: an unknown status, or `blocked` or
        // `cancelled` without a readable reason. Logged with the rest.
        bad.0.push("status");
    }

    let priority = i32::try_from(row.get::<_, i64>("priority")?).unwrap_or_else(|_| {
        bad.0.push("priority");
        0
    });

    let mut item = WorkItem {
        id: row.get("id")?,
        kind,
        title: row.get("title")?,
        objective: row.get("objective")?,
        done_when: row.get("done_when")?,
        constraints: bad.json("constraints", &row.get::<_, String>("constraints")?),
        decisions_made: bad.json("decisions_made", &row.get::<_, String>("decisions_made")?),
        artifact_refs: bad.json("artifact_refs", &row.get::<_, String>("artifact_refs")?),
        required_tools: bad.json("required_tools", &row.get::<_, String>("required_tools")?),
        required_mcp_servers: bad.json(
            "required_mcp_servers",
            &row.get::<_, String>("required_mcp_servers")?,
        ),
        worker_kind,
        writable_resources: bad.json(
            "writable_resources",
            &row.get::<_, String>("writable_resources")?,
        ),
        parent: row.get("parent")?,
        inputs_from: bad.json("inputs_from", &row.get::<_, String>("inputs_from")?),
        origin_conversation_id: row.get("origin_conversation_id")?,
        trigger: bad.json("trigger", &row.get::<_, String>("trigger")?),
        preconditions: bad.json("preconditions", &row.get::<_, String>("preconditions")?),
        expires_at: bad.time("expires_at", row.get("expires_at")?),
        budget: bad.json("budget", &row.get::<_, String>("budget")?),
        priority,
        status,
        status_origin: row.get("status_origin")?,
        plan_id: row.get("plan_id")?,
        held_by: row.get("held_by")?,
        created_at: bad.required_time("created_at", row.get("created_at")?),
        updated_at: bad.required_time("updated_at", row.get("updated_at")?),
        closed_at: bad.time("closed_at", row.get("closed_at")?),
    };

    if !bad.0.is_empty() {
        tracing::warn!(
            item = %item.id,
            columns = ?bad.0,
            "work item has unreadable columns; reading it as failed"
        );
        item.status = Status::Failed;
    }
    Ok(item)
}

/// Map a `SELECT item, depends_on, kind` row.
pub(super) fn edge_from_row(row: &Row) -> rusqlite::Result<Edge> {
    Ok(Edge {
        item: row.get("item")?,
        depends_on: row.get("depends_on")?,
        kind: EdgeKind::parse(&row.get::<_, String>("kind")?),
    })
}

fn status_of(name: Option<String>, reason: Option<String>) -> Option<Status> {
    name.map(|n| Status::parse(&n, reason.as_deref()))
}

/// Map a `SELECT {EVENT_COLUMNS}` row. An unknown event kind reads as
/// `warning`, a note that moves nothing, rather than failing the whole
/// history read.
pub(super) fn event_from_row(row: &Row) -> rusqlite::Result<WorkEvent> {
    let item: String = row.get("item")?;
    let kind_raw: String = row.get("kind")?;
    let kind = EventKind::parse(&kind_raw).unwrap_or_else(|| {
        tracing::warn!(item = %item, kind = %kind_raw, "unknown work event kind; reading it as warning");
        EventKind::Warning
    });
    let at_raw: String = row.get("at")?;
    Ok(WorkEvent {
        at: parse_ts(&at_raw).unwrap_or_default(),
        kind,
        from: status_of(row.get("from_status")?, row.get("from_reason")?),
        to: status_of(row.get("to_status")?, row.get("to_reason")?),
        actor: row.get("actor")?,
        reason: row.get("reason")?,
        upstream: row.get("upstream")?,
        origin: row.get("origin")?,
        evidence_ref: row.get("evidence_ref")?,
        item,
    })
}

pub(super) fn evidence_from_row(row: &Row) -> rusqlite::Result<Evidence> {
    let at_raw: String = row.get("at")?;
    Ok(Evidence {
        item: row.get("item")?,
        kind: row.get("kind")?,
        reference: row.get("ref")?,
        hash: row.get("hash")?,
        verified_by: row.get("verified_by")?,
        at: parse_ts(&at_raw).unwrap_or_default(),
    })
}

/// Map a `SELECT {LEASE_COLUMNS}` row. An unreadable heartbeat reads as
/// now: a lease the store cannot interpret is never treated as expired and
/// handed to a second worker while the first may still be running.
pub(super) fn lease_from_row(row: &Row) -> rusqlite::Result<Lease> {
    let item: String = row.get("item")?;
    let since_raw: String = row.get("since")?;
    let heartbeat_raw: String = row.get("heartbeat_at")?;
    let inputs_raw: String = row.get("inputs")?;
    let heartbeat_at = parse_ts(&heartbeat_raw).unwrap_or_else(|| {
        tracing::warn!(item = %item, "unreadable lease heartbeat; reading it as fresh");
        Utc::now()
    });
    Ok(Lease {
        since: parse_ts(&since_raw).unwrap_or(heartbeat_at),
        ttl_seconds: u64::try_from(row.get::<_, i64>("ttl_seconds")?).unwrap_or(0),
        heartbeat_at,
        inputs: serde_json::from_str(&inputs_raw).unwrap_or_else(|_| {
            tracing::warn!(item = %item, "unreadable lease inputs");
            Vec::new()
        }),
        worker: row.get("worker")?,
        item,
    })
}

pub(super) fn plan_from_row(row: &Row) -> rusqlite::Result<WorkPlanRow> {
    let created_raw: String = row.get("created_at")?;
    Ok(WorkPlanRow {
        id: row.get("id")?,
        root: row.get("root")?,
        filed_by: row.get("filed_by")?,
        rationale: row.get("rationale")?,
        approval_question: row.get("approval_question")?,
        policy: row.get("policy")?,
        created_at: parse_ts(&created_raw).unwrap_or_default(),
    })
}

pub(super) fn outbox_from_row(row: &Row) -> rusqlite::Result<OutboxRow> {
    let created_raw: String = row.get("created_at")?;
    let delivered_raw: Option<String> = row.get("delivered_at")?;
    Ok(OutboxRow {
        id: row.get("id")?,
        parent: row.get("parent")?,
        origin: row.get("origin")?,
        channel: row.get("channel")?,
        body: row.get("body")?,
        created_at: parse_ts(&created_raw).unwrap_or_default(),
        // A delivery stamp that does not parse still means delivered: a
        // notice must not be sent twice because a timestamp is damaged.
        delivered_at: delivered_raw.map(|raw| parse_ts(&raw).unwrap_or_default()),
    })
}

pub(super) fn archive_from_row(row: &Row) -> rusqlite::Result<ArchivedItem> {
    let kind_raw: String = row.get("kind")?;
    let status_raw: String = row.get("status")?;
    let reason: Option<String> = row.get("status_reason")?;
    let cost: Option<String> = row.get("cost")?;
    let closed_raw: String = row.get("closed_at")?;
    let archived_raw: String = row.get("archived_at")?;
    let edges_raw: String = row.get("edges")?;
    Ok(ArchivedItem {
        id: row.get("id")?,
        kind: WorkKind::parse(&kind_raw).unwrap_or(WorkKind::Internal),
        title: row.get("title")?,
        parent: row.get("parent")?,
        status: Status::parse(&status_raw, reason.as_deref()),
        worker: row.get("worker")?,
        cost: cost.and_then(|raw| serde_json::from_str(&raw).ok()),
        closed_at: parse_ts(&closed_raw).unwrap_or_default(),
        archived_at: parse_ts(&archived_raw).unwrap_or_default(),
        summary: row.get("summary")?,
        edges: serde_json::from_str(&edges_raw).unwrap_or_default(),
    })
}
