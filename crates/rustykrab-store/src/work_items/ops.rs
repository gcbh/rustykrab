//! The synchronous half: every read and write as a function of one
//! connection, so the async methods in `mod.rs` and [`apply`] can compose
//! them inside one transaction. Nothing here opens a transaction itself.

use chrono::{DateTime, TimeDelta, Utc};
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Params};
use uuid::Uuid;

use rustykrab_core::work::{
    ArtifactRef, Edge, EventKind, Evidence, InputRef, Lease, Status, Trigger, WorkEvent, WorkItem,
    WorkKind,
};

use super::rows::{
    archive_from_row, edge_from_row, event_from_row, evidence_from_row, item_from_row,
    lease_from_row, outbox_from_row, plan_from_row, to_json, ts, ARCHIVE_COLUMNS, EVENT_COLUMNS,
    EVIDENCE_COLUMNS, ITEM_COLUMNS, LEASE_COLUMNS, OUTBOX_COLUMNS, PLAN_COLUMNS,
};
use super::{
    ArchivedItem, LeaseRecord, OutboxDraft, OutboxRow, RepointSpec, RunSpend, Spend,
    TransitionSpec, WorkApplied, WorkFilter, WorkOp, WorkPlanRow, WorkStoreError,
};

/// The closed statuses, as SQL, for "open by the status column".
const CLOSED_SQL: &str = "('done', 'failed', 'cancelled', 'expired')";

/// The edge kinds that record history rather than order work.
const HISTORY_KINDS_SQL: &str = "('supersedes', 'discovered_from')";

fn collect<T, P, F>(
    conn: &Connection,
    sql: &str,
    params: P,
    map: F,
) -> Result<Vec<T>, WorkStoreError>
where
    P: Params,
    F: FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
{
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params, map)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Apply one op of a [`super::Store::work_apply`] batch.
pub(super) fn apply(
    conn: &Connection,
    op: &WorkOp,
    now: DateTime<Utc>,
    applied: &mut WorkApplied,
) -> Result<(), WorkStoreError> {
    match op {
        WorkOp::Insert(item) => insert_item(conn, item),
        WorkOp::Plan(plan) => insert_plan(conn, plan),
        WorkOp::AddEdge(edge) => add_edge(conn, edge).map(|_| ()),
        WorkOp::RemoveOrderingEdgesOf(item) => remove_ordering_edges_of(conn, item).map(|_| ()),
        WorkOp::Repoint(spec) => {
            applied.events.push(repoint(conn, spec, now)?);
            Ok(())
        }
        WorkOp::Transition(spec) => {
            applied.events.push(transition(conn, spec, now)?);
            Ok(())
        }
        WorkOp::Note(event) => {
            append_note(conn, event)?;
            applied.events.push(event.clone());
            Ok(())
        }
        WorkOp::Outbox(draft) => {
            applied.outbox_ids.push(enqueue_outbox(conn, draft, now)?);
            Ok(())
        }
        WorkOp::Facets { item, facets } => {
            crate::proposals::put_facets(conn, item, facets).map_err(WorkStoreError::from)
        }
        WorkOp::ReleaseHold(item) => release_hold(conn, item, now),
        WorkOp::AddArtifactRef { item, artifact } => add_artifact_ref(conn, item, artifact, now),
        WorkOp::Question(write) => crate::questions::apply_question(conn, write),
        WorkOp::Notice(notice) => {
            applied
                .outbox_ids
                .push(crate::questions::apply_notice(conn, notice, now)?);
            Ok(())
        }
    }
}

/// Append `artifact` to an item's refs unless it is already there.
/// Refused for an id no live item has.
pub(super) fn add_artifact_ref(
    conn: &Connection,
    item: &str,
    artifact: &ArtifactRef,
    now: DateTime<Utc>,
) -> Result<(), WorkStoreError> {
    let Some(mut row) = get_item(conn, item)? else {
        return Err(WorkStoreError::NotFound(format!("work item {item}")));
    };
    if row.artifact_refs.contains(artifact) {
        return Ok(());
    }
    row.artifact_refs.push(artifact.clone());
    conn.execute(
        "UPDATE work_items SET artifact_refs = ?2, updated_at = ?3 WHERE id = ?1",
        params![item, to_json(&row.artifact_refs)?, ts(&now)],
    )?;
    Ok(())
}

/// Clear an item's approval hold. Refused for an id no live item has.
pub(super) fn release_hold(
    conn: &Connection,
    item: &str,
    now: DateTime<Utc>,
) -> Result<(), WorkStoreError> {
    let cleared = conn.execute(
        "UPDATE work_items SET held_by = NULL, updated_at = ?2 WHERE id = ?1",
        params![item, ts(&now)],
    )?;
    if cleared == 0 {
        return Err(WorkStoreError::NotFound(format!("work item {item}")));
    }
    Ok(())
}

// ── items ──────────────────────────────────────────────────────────────

pub(super) fn get_item(conn: &Connection, id: &str) -> Result<Option<WorkItem>, WorkStoreError> {
    Ok(conn
        .query_row(
            &format!("SELECT {ITEM_COLUMNS} FROM work_items WHERE id = ?1"),
            params![id],
            item_from_row,
        )
        .optional()?)
}

fn require_item(conn: &Connection, id: &str) -> Result<WorkItem, WorkStoreError> {
    get_item(conn, id)?.ok_or_else(|| WorkStoreError::NotFound(format!("work item {id}")))
}

fn item_exists(conn: &Connection, id: &str) -> Result<bool, WorkStoreError> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM work_items WHERE id = ?1",
            params![id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

pub(super) fn insert_item(conn: &Connection, item: &WorkItem) -> Result<(), WorkStoreError> {
    if item_exists(conn, &item.id)? {
        return Err(WorkStoreError::AlreadyExists(format!(
            "work item {}",
            item.id
        )));
    }
    let trigger_at = match &item.trigger {
        Trigger::At(at) => Some(ts(at)),
        _ => None,
    };
    let placeholders = (1..=29)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    // Bound in ITEM_COLUMNS order.
    conn.execute(
        &format!("INSERT INTO work_items ({ITEM_COLUMNS}) VALUES ({placeholders})"),
        params![
            item.id,
            item.kind.as_str(),
            item.title,
            item.objective,
            item.done_when,
            item.status.name(),
            item.status.reason(),
            item.status_origin,
            item.priority,
            item.parent,
            item.worker_kind.as_str(),
            item.origin_conversation_id,
            trigger_at,
            item.expires_at.as_ref().map(ts),
            item.plan_id,
            item.held_by,
            ts(&item.created_at),
            ts(&item.updated_at),
            item.closed_at.as_ref().map(ts),
            to_json(&item.constraints)?,
            to_json(&item.decisions_made)?,
            to_json(&item.artifact_refs)?,
            to_json(&item.required_tools)?,
            to_json(&item.required_mcp_servers)?,
            to_json(&item.writable_resources)?,
            to_json(&item.inputs_from)?,
            to_json(&item.preconditions)?,
            to_json(&item.budget)?,
            to_json(&item.trigger)?,
        ],
    )?;
    Ok(())
}

pub(super) fn list_items(
    conn: &Connection,
    filter: &WorkFilter,
) -> Result<Vec<WorkItem>, WorkStoreError> {
    let mut clauses: Vec<String> = Vec::new();
    let mut values: Vec<Value> = Vec::new();
    // `test ?n`, binding `value` as parameter n. `status_reason IS ?n`
    // matches NULL for the statuses that take no reason.
    fn bind(clauses: &mut Vec<String>, values: &mut Vec<Value>, test: &str, value: Option<String>) {
        values.push(value.map_or(Value::Null, Value::Text));
        clauses.push(format!("{test} ?{}", values.len()));
    }
    match &filter.status {
        Some(status) => {
            bind(
                &mut clauses,
                &mut values,
                "status =",
                Some(status.name().to_string()),
            );
            let reason = status.reason().map(str::to_string);
            bind(&mut clauses, &mut values, "status_reason IS", reason);
        }
        None if !filter.include_closed => clauses.push(format!("status NOT IN {CLOSED_SQL}")),
        None => {}
    }
    if let Some(kind) = filter.kind {
        bind(
            &mut clauses,
            &mut values,
            "kind =",
            Some(kind.as_str().to_string()),
        );
    }
    if let Some(parent) = &filter.parent {
        bind(&mut clauses, &mut values, "parent =", Some(parent.clone()));
    }
    let predicate = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    collect(
        conn,
        &format!("SELECT {ITEM_COLUMNS} FROM work_items {predicate} ORDER BY created_at, id"),
        params_from_iter(values.iter()),
        item_from_row,
    )
}

// ── plans ──────────────────────────────────────────────────────────────

pub(super) fn insert_plan(conn: &Connection, plan: &WorkPlanRow) -> Result<(), WorkStoreError> {
    let changed = conn.execute(
        &format!(
            "INSERT OR IGNORE INTO work_plans ({PLAN_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
        ),
        params![
            plan.id,
            plan.root,
            plan.filed_by,
            plan.rationale,
            plan.approval_question,
            plan.policy,
            ts(&plan.created_at),
        ],
    )?;
    if changed == 0 {
        return Err(WorkStoreError::AlreadyExists(format!(
            "work plan {}",
            plan.id
        )));
    }
    Ok(())
}

pub(super) fn plan_get(conn: &Connection, id: &str) -> Result<Option<WorkPlanRow>, WorkStoreError> {
    Ok(conn
        .query_row(
            &format!("SELECT {PLAN_COLUMNS} FROM work_plans WHERE id = ?1"),
            params![id],
            plan_from_row,
        )
        .optional()?)
}

// ── edges ──────────────────────────────────────────────────────────────

/// Add one edge. The downstream must be live; the upstream need not be (an
/// edge onto an archived item is history). `true` if the row is new.
pub(super) fn add_edge(conn: &Connection, edge: &Edge) -> Result<bool, WorkStoreError> {
    if !item_exists(conn, &edge.item)? {
        return Err(WorkStoreError::NotFound(format!("work item {}", edge.item)));
    }
    let added = conn.execute(
        "INSERT OR IGNORE INTO work_item_deps (item, depends_on, kind) VALUES (?1, ?2, ?3)",
        params![edge.item, edge.depends_on, edge.kind.as_str()],
    )?;
    Ok(added == 1)
}

pub(super) fn edges_of(conn: &Connection, item: &str) -> Result<Vec<Edge>, WorkStoreError> {
    collect(
        conn,
        "SELECT item, depends_on, kind FROM work_item_deps WHERE item = ?1
          ORDER BY depends_on, kind",
        params![item],
        edge_from_row,
    )
}

pub(super) fn dependents_of(
    conn: &Connection,
    upstream: &str,
) -> Result<Vec<Edge>, WorkStoreError> {
    collect(
        conn,
        "SELECT item, depends_on, kind FROM work_item_deps WHERE depends_on = ?1
          ORDER BY item, kind",
        params![upstream],
        edge_from_row,
    )
}

pub(super) fn edges_all_open(conn: &Connection) -> Result<Vec<Edge>, WorkStoreError> {
    collect(
        conn,
        &format!(
            "SELECT item, depends_on, kind FROM work_item_deps
              WHERE item IN (SELECT id FROM work_items WHERE status NOT IN {CLOSED_SQL})
                 OR depends_on IN (SELECT id FROM work_items WHERE status NOT IN {CLOSED_SQL})
              ORDER BY item, depends_on, kind"
        ),
        [],
        edge_from_row,
    )
}

/// Drop the ordering edges `item` holds. A kind the store cannot read counts
/// as ordering, since it reads as `blocks`.
pub(super) fn remove_ordering_edges_of(
    conn: &Connection,
    item: &str,
) -> Result<usize, WorkStoreError> {
    Ok(conn.execute(
        &format!("DELETE FROM work_item_deps WHERE item = ?1 AND kind NOT IN {HISTORY_KINDS_SQL}"),
        params![item],
    )?)
}

/// Move an edge (when `spec.kind` is set) and the matching `inputs_from`
/// entry from the old upstream to the new one, and record a `repoint` event
/// whose `upstream` is the old value: the deps row now holds only the new
/// one, so the event is where the old one lives.
pub(super) fn repoint(
    conn: &Connection,
    spec: &RepointSpec,
    now: DateTime<Utc>,
) -> Result<WorkEvent, WorkStoreError> {
    let item = require_item(conn, &spec.item)?;
    let (old, new) = (&spec.old_upstream, &spec.new_upstream);

    if let Some(kind) = spec.kind {
        let moved = conn.execute(
            "UPDATE OR IGNORE work_item_deps SET depends_on = ?3
              WHERE item = ?1 AND depends_on = ?2 AND kind = ?4",
            params![spec.item, old, new, kind.as_str()],
        )?;
        if moved == 0 {
            // Either the edge is not there, or the new edge already is and
            // the update was ignored; then the old row is the duplicate.
            let dropped = conn.execute(
                "DELETE FROM work_item_deps WHERE item = ?1 AND depends_on = ?2 AND kind = ?3",
                params![spec.item, old, kind.as_str()],
            )?;
            if dropped == 0 {
                return Err(WorkStoreError::NotFound(format!(
                    "{} edge from {} to {old}",
                    kind.as_str(),
                    spec.item
                )));
            }
        }
    }

    let names_old = item.inputs_from.iter().any(|i| i == old);
    if names_old {
        let mut inputs: Vec<String> = Vec::with_capacity(item.inputs_from.len());
        for input in &item.inputs_from {
            let next = if input == old { new } else { input };
            if !inputs.contains(next) {
                inputs.push(next.clone());
            }
        }
        conn.execute(
            "UPDATE work_items SET inputs_from = ?2 WHERE id = ?1",
            params![spec.item, to_json(&inputs)?],
        )?;
    } else if spec.kind.is_none() {
        return Err(WorkStoreError::NotFound(format!(
            "input {old} on work item {}",
            spec.item
        )));
    }
    conn.execute(
        "UPDATE work_items SET updated_at = ?2 WHERE id = ?1",
        params![spec.item, ts(&now)],
    )?;

    let what = match (spec.kind, names_old) {
        (Some(kind), true) => format!("{} edge and input", kind.as_str()),
        (Some(kind), false) => format!("{} edge", kind.as_str()),
        (None, _) => "input".to_string(),
    };
    let event = WorkEvent {
        item: spec.item.clone(),
        at: now,
        kind: EventKind::Repoint,
        from: None,
        to: None,
        actor: spec.actor.clone(),
        reason: Some(format!("{what} re-pointed from {old} to {new}")),
        upstream: Some(old.clone()),
        origin: spec.origin.clone(),
        evidence_ref: None,
    };
    insert_event(conn, &event)?;
    Ok(event)
}

// ── transitions and events ─────────────────────────────────────────────

/// Write one status change and its event. Closed is final; a stale
/// `expected_from` is refused. Entering a closed status stamps `closed_at`;
/// entering any status that is not active drops the item's lease.
pub(super) fn transition(
    conn: &Connection,
    spec: &TransitionSpec,
    now: DateTime<Utc>,
) -> Result<WorkEvent, WorkStoreError> {
    let current = require_item(conn, &spec.item)?.status;
    if current.is_closed() {
        return Err(WorkStoreError::Closed {
            item: spec.item.clone(),
            status: current,
        });
    }
    if let Some(expected) = spec.expected_from {
        if expected != current {
            return Err(WorkStoreError::StatusMismatch {
                item: spec.item.clone(),
                expected,
                actual: current,
            });
        }
    }

    let stamp = ts(&now);
    conn.execute(
        "UPDATE work_items
            SET status = ?2, status_reason = ?3, status_origin = ?4,
                updated_at = ?5, closed_at = ?6
          WHERE id = ?1",
        params![
            spec.item,
            spec.to.name(),
            spec.to.reason(),
            spec.origin,
            stamp,
            spec.to.is_closed().then_some(&stamp),
        ],
    )?;
    if !spec.to.is_active() {
        end_lease(conn, &spec.item, now)?;
    }

    let event = WorkEvent {
        item: spec.item.clone(),
        at: now,
        kind: spec.kind,
        from: Some(current),
        to: Some(spec.to),
        actor: spec.actor.clone(),
        reason: spec.reason.clone(),
        upstream: spec.upstream.clone(),
        origin: spec.origin.clone(),
        evidence_ref: spec.evidence_ref.clone(),
    };
    insert_event(conn, &event)?;
    Ok(event)
}

fn insert_event(conn: &Connection, event: &WorkEvent) -> Result<(), WorkStoreError> {
    conn.execute(
        &format!(
            "INSERT INTO work_item_events ({EVENT_COLUMNS})
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"
        ),
        params![
            event.item,
            ts(&event.at),
            event.kind.as_str(),
            event.from.map(|s| s.name()),
            event.from.and_then(|s| s.reason()),
            event.to.map(|s| s.name()),
            event.to.and_then(|s| s.reason()),
            event.actor,
            event.reason,
            event.upstream,
            event.origin,
            event.evidence_ref,
        ],
    )?;
    Ok(())
}

/// Append an event that changes no status, on a live item.
pub(super) fn append_note(conn: &Connection, event: &WorkEvent) -> Result<(), WorkStoreError> {
    if event.to.is_some() {
        return Err(WorkStoreError::EventCarriesStatus {
            item: event.item.clone(),
            kind: event.kind,
        });
    }
    if !item_exists(conn, &event.item)? {
        return Err(WorkStoreError::NotFound(format!(
            "work item {}",
            event.item
        )));
    }
    insert_event(conn, event)
}

pub(super) fn events_of(conn: &Connection, item: &str) -> Result<Vec<WorkEvent>, WorkStoreError> {
    collect(
        conn,
        &format!("SELECT {EVENT_COLUMNS} FROM work_item_events WHERE item = ?1 ORDER BY at, id"),
        params![item],
        event_from_row,
    )
}

/// Events with a row id above `after`, in row order, each with its id: a
/// cursor that, unlike a timestamp, never repeats or skips a row written
/// in the same millisecond.
pub(super) fn events_after(
    conn: &Connection,
    after: i64,
    limit: usize,
) -> Result<Vec<(i64, WorkEvent)>, WorkStoreError> {
    collect(
        conn,
        &format!(
            "SELECT id, {EVENT_COLUMNS} FROM work_item_events WHERE id > ?1 ORDER BY id LIMIT ?2"
        ),
        params![after, i64::try_from(limit).unwrap_or(i64::MAX)],
        |row| Ok((row.get::<_, i64>("id")?, event_from_row(row)?)),
    )
}

/// The id of the newest event, 0 when there is none.
pub(super) fn events_last_id(conn: &Connection) -> Result<i64, WorkStoreError> {
    Ok(conn.query_row(
        "SELECT COALESCE(MAX(id), 0) FROM work_item_events",
        [],
        |row| row.get(0),
    )?)
}

pub(super) fn events_since(
    conn: &Connection,
    at: DateTime<Utc>,
) -> Result<Vec<WorkEvent>, WorkStoreError> {
    collect(
        conn,
        &format!("SELECT {EVENT_COLUMNS} FROM work_item_events WHERE at >= ?1 ORDER BY at, id"),
        params![ts(&at)],
        event_from_row,
    )
}

// ── evidence ───────────────────────────────────────────────────────────

pub(super) fn add_evidence(conn: &Connection, evidence: &Evidence) -> Result<(), WorkStoreError> {
    if !item_exists(conn, &evidence.item)? {
        return Err(WorkStoreError::NotFound(format!(
            "work item {}",
            evidence.item
        )));
    }
    conn.execute(
        &format!(
            "INSERT INTO work_item_evidence ({EVIDENCE_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
        ),
        params![
            evidence.item,
            evidence.kind,
            evidence.reference,
            evidence.hash,
            evidence.verified_by,
            ts(&evidence.at),
        ],
    )?;
    Ok(())
}

pub(super) fn evidence_of(conn: &Connection, item: &str) -> Result<Vec<Evidence>, WorkStoreError> {
    collect(
        conn,
        &format!(
            "SELECT {EVIDENCE_COLUMNS} FROM work_item_evidence WHERE item = ?1 ORDER BY at, id"
        ),
        params![item],
        evidence_from_row,
    )
}

pub(super) fn evidence_of_kind(
    conn: &Connection,
    kind: &str,
) -> Result<Vec<Evidence>, WorkStoreError> {
    collect(
        conn,
        &format!(
            "SELECT {EVIDENCE_COLUMNS} FROM work_item_evidence WHERE kind = ?1 ORDER BY at, id"
        ),
        params![kind],
        evidence_from_row,
    )
}

// ── leases ─────────────────────────────────────────────────────────────

pub(super) fn get_lease(conn: &Connection, item: &str) -> Result<Option<Lease>, WorkStoreError> {
    Ok(conn
        .query_row(
            &format!("SELECT {LEASE_COLUMNS} FROM leases WHERE item = ?1"),
            params![item],
            lease_from_row,
        )
        .optional()?)
}

/// Lease a `ready` item: the lease row, `ready -> leased`, and a `lease`
/// event whose actor is the worker, so the archive can name who did the
/// work.
pub(super) fn lease_acquire(
    conn: &Connection,
    item_id: &str,
    worker: &str,
    ttl_seconds: u64,
    inputs: Vec<InputRef>,
    now: DateTime<Utc>,
) -> Result<Lease, WorkStoreError> {
    let item = require_item(conn, item_id)?;
    if item.status.is_closed() {
        return Err(WorkStoreError::Closed {
            item: item.id,
            status: item.status,
        });
    }
    if let Some(held) = get_lease(conn, item_id)? {
        return Err(WorkStoreError::LeaseHeld {
            item: item.id,
            worker: held.worker,
        });
    }
    if item.status != Status::Ready {
        return Err(WorkStoreError::StatusMismatch {
            item: item.id,
            expected: Status::Ready,
            actual: item.status,
        });
    }

    let lease = Lease {
        item: item.id.clone(),
        worker: worker.to_string(),
        since: now,
        ttl_seconds,
        heartbeat_at: now,
        inputs,
    };
    conn.execute(
        &format!("INSERT INTO leases ({LEASE_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"),
        params![
            lease.item,
            lease.worker,
            ts(&lease.since),
            i64::try_from(ttl_seconds).unwrap_or(i64::MAX),
            ts(&lease.heartbeat_at),
            to_json(&lease.inputs)?,
        ],
    )?;
    conn.execute(
        "UPDATE work_items
            SET status = 'leased', status_reason = NULL, status_origin = NULL, updated_at = ?2
          WHERE id = ?1",
        params![item.id, ts(&now)],
    )?;
    insert_event(
        conn,
        &WorkEvent {
            item: item.id.clone(),
            at: now,
            kind: EventKind::Lease,
            from: Some(Status::Ready),
            to: Some(Status::Leased),
            actor: format!("worker:{worker}"),
            reason: Some(format!("ttl {ttl_seconds}s")),
            upstream: None,
            origin: None,
            evidence_ref: None,
        },
    )?;
    Ok(lease)
}

pub(super) fn lease_heartbeat(
    conn: &Connection,
    item: &str,
    now: DateTime<Utc>,
) -> Result<Lease, WorkStoreError> {
    let beat = conn.execute(
        "UPDATE leases SET heartbeat_at = ?2 WHERE item = ?1",
        params![item, ts(&now)],
    )?;
    if beat == 0 {
        return Err(WorkStoreError::NotFound(format!("lease on {item}")));
    }
    get_lease(conn, item)?.ok_or_else(|| WorkStoreError::NotFound(format!("lease on {item}")))
}

pub(super) fn lease_release(
    conn: &Connection,
    item: &str,
    now: DateTime<Utc>,
) -> Result<Option<Lease>, WorkStoreError> {
    end_lease(conn, item, now)
}

/// End the item's lease, if it has one: copy it into
/// `work_lease_history` with `now` as its end, then drop the live row.
/// Every path that drops a lease comes through here, so no lease leaves
/// the record.
fn end_lease(
    conn: &Connection,
    item: &str,
    now: DateTime<Utc>,
) -> Result<Option<Lease>, WorkStoreError> {
    let Some(lease) = get_lease(conn, item)? else {
        return Ok(None);
    };
    conn.execute(
        &format!(
            "INSERT INTO work_lease_history ({LEASE_COLUMNS}, released_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
        ),
        params![
            lease.item,
            lease.worker,
            ts(&lease.since),
            i64::try_from(lease.ttl_seconds).unwrap_or(i64::MAX),
            ts(&lease.heartbeat_at),
            to_json(&lease.inputs)?,
            ts(&now),
        ],
    )?;
    conn.execute("DELETE FROM leases WHERE item = ?1", params![item])?;
    Ok(Some(lease))
}

/// Every lease `item` held: the history, oldest first, then the live one.
pub(super) fn lease_history(
    conn: &Connection,
    item: &str,
) -> Result<Vec<LeaseRecord>, WorkStoreError> {
    let mut out = collect(
        conn,
        &format!(
            "SELECT {LEASE_COLUMNS}, released_at FROM work_lease_history
              WHERE item = ?1 ORDER BY since, id"
        ),
        params![item],
        |row| {
            let released: String = row.get("released_at")?;
            Ok(LeaseRecord {
                lease: lease_from_row(row)?,
                released_at: super::rows::parse_ts(&released),
            })
        },
    )?;
    if let Some(live) = get_lease(conn, item)? {
        out.push(LeaseRecord {
            lease: live,
            released_at: None,
        });
    }
    Ok(out)
}

// ── spend ──────────────────────────────────────────────────────────────

pub(super) fn record_spend(conn: &Connection, spend: &RunSpend) -> Result<(), WorkStoreError> {
    conn.execute(
        "INSERT INTO work_spend (item, run, worker, tokens, wall_ms, iterations, at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            spend.item,
            spend.run,
            spend.worker,
            i64::try_from(spend.tokens).unwrap_or(i64::MAX),
            i64::try_from(spend.wall_ms).unwrap_or(i64::MAX),
            i64::from(spend.iterations),
            ts(&spend.at),
        ],
    )?;
    Ok(())
}

fn spend_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Spend> {
    let n = |i: usize| -> rusqlite::Result<u64> {
        Ok(u64::try_from(row.get::<_, i64>(i)?).unwrap_or(0))
    };
    Ok(Spend {
        runs: u32::try_from(n(0)?).unwrap_or(u32::MAX),
        tokens: n(1)?,
        wall_ms: n(2)?,
        iterations: n(3)?,
    })
}

pub(super) fn spend_of(conn: &Connection, item: &str) -> Result<Spend, WorkStoreError> {
    Ok(conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(tokens), 0), COALESCE(SUM(wall_ms), 0),
                COALESCE(SUM(iterations), 0)
           FROM work_spend WHERE item = ?1",
        params![item],
        spend_from_row,
    )?)
}

pub(super) fn spend_totals(
    conn: &Connection,
) -> Result<std::collections::HashMap<String, Spend>, WorkStoreError> {
    let rows = collect(
        conn,
        "SELECT COUNT(*), COALESCE(SUM(tokens), 0), COALESCE(SUM(wall_ms), 0),
                COALESCE(SUM(iterations), 0), item
           FROM work_spend GROUP BY item",
        [],
        |row| Ok((row.get::<_, String>(4)?, spend_from_row(row)?)),
    )?;
    Ok(rows.into_iter().collect())
}

/// Leases are few (one per active leaf), so the expiry test runs in Rust,
/// where the arithmetic is exact, rather than in SQL date functions.
pub(super) fn leases_expired(
    conn: &Connection,
    now: DateTime<Utc>,
) -> Result<Vec<Lease>, WorkStoreError> {
    let all = collect(
        conn,
        &format!("SELECT {LEASE_COLUMNS} FROM leases ORDER BY heartbeat_at, item"),
        [],
        lease_from_row,
    )?;
    Ok(all
        .into_iter()
        .filter(|lease| {
            let ttl = TimeDelta::try_seconds(i64::try_from(lease.ttl_seconds).unwrap_or(i64::MAX))
                .unwrap_or(TimeDelta::MAX);
            // A deadline past the end of time never expires.
            lease
                .heartbeat_at
                .checked_add_signed(ttl)
                .is_some_and(|deadline| deadline < now)
        })
        .collect())
}

// ── outbox ─────────────────────────────────────────────────────────────

pub(super) fn enqueue_outbox(
    conn: &Connection,
    draft: &OutboxDraft,
    now: DateTime<Utc>,
) -> Result<String, WorkStoreError> {
    let id = Uuid::new_v4().to_string();
    conn.execute(
        &format!(
            "INSERT INTO work_outbox ({OUTBOX_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)"
        ),
        params![
            id,
            draft.parent,
            draft.origin,
            draft.channel,
            draft.body,
            ts(&now)
        ],
    )?;
    Ok(id)
}

/// Undelivered notices that are due: a notice with a `not_before` still in
/// the future is waiting for its coalescing window (section 6.6).
pub(super) fn outbox_pending(conn: &Connection) -> Result<Vec<OutboxRow>, WorkStoreError> {
    collect(
        conn,
        &format!(
            "SELECT {OUTBOX_COLUMNS} FROM work_outbox WHERE delivered_at IS NULL
                AND (not_before IS NULL OR not_before <= ?1)
              ORDER BY created_at, rowid"
        ),
        params![ts(&Utc::now())],
        outbox_from_row,
    )
}

/// Bounded WebChat notice history for work owned by this conversation.
/// Reads include consumed notices, and never deliver or advance work.
pub(super) fn outbox_for_conversation(
    conn: &Connection,
    conversation: &str,
    limit: usize,
) -> Result<Vec<OutboxRow>, WorkStoreError> {
    let mut rows = collect(
        conn,
        "SELECT o.id,o.parent,o.origin,o.channel,o.body,o.created_at,o.delivered_at
         FROM work_outbox o JOIN work_items w ON w.id=o.parent
         WHERE w.origin_conversation_id=?1 AND o.channel='webchat'
           AND (o.not_before IS NULL OR o.not_before<=?2)
         ORDER BY o.created_at DESC,o.rowid DESC LIMIT ?3",
        params![conversation, ts(&Utc::now()), limit.min(100) as i64],
        outbox_from_row,
    )?;
    rows.reverse();
    Ok(rows)
}

pub(super) fn outbox_mark_delivered(
    conn: &Connection,
    id: &str,
    now: DateTime<Utc>,
) -> Result<bool, WorkStoreError> {
    let marked = conn.execute(
        "UPDATE work_outbox SET delivered_at = ?2 WHERE id = ?1 AND delivered_at IS NULL",
        params![id, ts(&now)],
    )?;
    if marked == 1 {
        return Ok(true);
    }
    let exists = conn
        .query_row(
            "SELECT 1 FROM work_outbox WHERE id = ?1",
            params![id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if exists {
        Ok(false)
    } else {
        Err(WorkStoreError::NotFound(format!("outbox row {id}")))
    }
}

// ── archive ────────────────────────────────────────────────────────────

/// The worker that last held the item's lease, from its `lease` events.
fn last_worker(conn: &Connection, item: &str) -> Result<Option<String>, WorkStoreError> {
    let actor: Option<String> = conn
        .query_row(
            "SELECT actor FROM work_item_events WHERE item = ?1 AND kind = 'lease'
              ORDER BY at DESC, id DESC LIMIT 1",
            params![item],
            |row| row.get(0),
        )
        .optional()?;
    Ok(actor.map(|a| a.strip_prefix("worker:").unwrap_or(&a).to_string()))
}

/// The one-line summary, from typed fields only:
/// `title  kind  status[ <- origin][; worker name]`, the shape of a line in
/// `work show --graph` (section 14.2).
fn archive_summary(item: &WorkItem, worker: Option<&str>) -> String {
    const MAX_TITLE: usize = 120;
    let flat: String = item.title.split_whitespace().collect::<Vec<_>>().join(" ");
    let title = match flat.char_indices().nth(MAX_TITLE) {
        Some((cut, _)) => format!("{}...", &flat[..cut]),
        None => flat,
    };
    let mut line = format!("{title}  {}  {}", item.kind.as_str(), item.status);
    if let Some(origin) = &item.status_origin {
        line.push_str(&format!(" <- {origin}"));
    }
    if let Some(worker) = worker {
        line.push_str(&format!("; worker {worker}"));
    }
    line
}

fn archived(conn: &Connection, id: &str) -> Result<bool, WorkStoreError> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM work_item_archive WHERE id = ?1",
            params![id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Compact one closed item: the archive line in, the live row and the edges
/// it holds out. Events, evidence and edges naming it stay. `false` if it
/// was already archived.
pub(super) fn archive_one(
    conn: &Connection,
    id: &str,
    now: DateTime<Utc>,
) -> Result<bool, WorkStoreError> {
    let Some(item) = get_item(conn, id)? else {
        if archived(conn, id)? {
            return Ok(false);
        }
        return Err(WorkStoreError::NotFound(format!("work item {id}")));
    };
    if !item.status.is_closed() {
        return Err(WorkStoreError::NotClosed {
            item: item.id,
            status: item.status,
        });
    }
    let edges = edges_of(conn, id)?;
    let worker = last_worker(conn, id)?;
    let summary = archive_summary(&item, worker.as_deref());
    // What its runs spent; NULL when no run recorded any.
    let spent = spend_of(conn, id)?;
    let cost = if spent.runs > 0 {
        Some(to_json(&spent)?)
    } else {
        None
    };
    conn.execute(
        &format!(
            "INSERT INTO work_item_archive ({ARCHIVE_COLUMNS})
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"
        ),
        params![
            item.id,
            item.kind.as_str(),
            item.title,
            item.parent,
            item.status.name(),
            item.status.reason(),
            worker,
            cost,
            ts(&item.closed_at.unwrap_or(item.updated_at)),
            ts(&now),
            summary,
            to_json(&edges)?,
        ],
    )?;
    conn.execute("DELETE FROM work_item_deps WHERE item = ?1", params![id])?;
    end_lease(conn, id, now)?;
    conn.execute("DELETE FROM work_items WHERE id = ?1", params![id])?;
    Ok(true)
}

pub(super) fn archive_get(
    conn: &Connection,
    id: &str,
) -> Result<Option<ArchivedItem>, WorkStoreError> {
    Ok(conn
        .query_row(
            &format!("SELECT {ARCHIVE_COLUMNS} FROM work_item_archive WHERE id = ?1"),
            params![id],
            archive_from_row,
        )
        .optional()?)
}

pub(super) fn archive_list(
    conn: &Connection,
    kind: Option<WorkKind>,
    since: Option<DateTime<Utc>>,
) -> Result<Vec<ArchivedItem>, WorkStoreError> {
    collect(
        conn,
        &format!(
            "SELECT {ARCHIVE_COLUMNS} FROM work_item_archive
              WHERE (?1 IS NULL OR kind = ?1) AND (?2 IS NULL OR closed_at >= ?2)
              ORDER BY closed_at DESC, id"
        ),
        params![kind.map(|k| k.as_str()), since.as_ref().map(ts)],
        archive_from_row,
    )
}

pub(super) fn archive_search(
    conn: &Connection,
    text: &str,
) -> Result<Vec<ArchivedItem>, WorkStoreError> {
    let escaped = text
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    collect(
        conn,
        &format!(
            "SELECT {ARCHIVE_COLUMNS} FROM work_item_archive
              WHERE id = ?1
                 OR title LIKE ?2 ESCAPE '\\'
                 OR summary LIKE ?2 ESCAPE '\\'
              ORDER BY closed_at DESC, id"
        ),
        params![text, format!("%{escaped}%")],
        archive_from_row,
    )
}
