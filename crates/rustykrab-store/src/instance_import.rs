//! Offline consolidation of an operator-owned, normalized instance snapshot.
//! Execution ownership remains with the destination: old worker registrations,
//! routing policies, live leases and pairing codes are never activated by import.
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, OpenFlags, OptionalExtension};
use rustykrab_core::Error;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::{with_conn, SecretStore, Store};

const DATA_TABLES: &[&str] = &[
    "conversations",
    "messages",
    "recall_archive",
    "channel_bindings",
    "channel_inbound",
    "scheduled_jobs",
    "job_runs",
    "secrets",
    "secret_versions",
    "secret_audit",
    "credential_requests",
    "payment_requests",
    "devices",
    "delegated_tasks",
    "outcome_records",
    "outcome_attributions",
    "dream_reports",
    "dream_cycles",
    "dream_changes",
    "projects",
    "project_revisions",
    "plan_nodes",
    "plan_edges",
    "work_items",
    "work_item_deps",
    "work_item_events",
    "work_item_evidence",
    "work_item_archive",
    "work_plans",
    "work_outbox",
    "work_lease_history",
    "work_spend",
    "work_item_facets",
    "proposals",
    "proposal_evidence",
    "expectation_metrics",
    "work_projections",
    "questions",
    "judgment_policies",
    "dream_project_reviews",
];
const RUNTIME_TABLES: &[&str] = &["workers", "routing_defaults", "leases", "pairing_codes"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceImportReport {
    pub source: String,
    pub fingerprint: String,
    pub imported_at: String,
    pub inserted: BTreeMap<String, usize>,
    pub reused: BTreeMap<String, usize>,
    pub retained_in_snapshot: BTreeMap<String, usize>,
    pub integer_id_offsets: BTreeMap<String, i64>,
    pub previously_enabled_schedules: Vec<String>,
    pub retired_pending_notices: usize,
    pub reencrypted_values: usize,
}

pub(crate) fn migrate(conn: &Connection) -> Result<(), Error> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS instance_imports (
        source TEXT PRIMARY KEY, fingerprint TEXT NOT NULL, report TEXT NOT NULL,
        imported_at TEXT NOT NULL
    );",
    )
    .map_err(storage)?;
    let columns = columns(conn, "work_outbox")?;
    if !columns.iter().any(|column| column.name == "retired_at") {
        conn.execute("ALTER TABLE work_outbox ADD COLUMN retired_at TEXT", [])
            .map_err(storage)?;
    }
    Ok(())
}

fn storage(error: rusqlite::Error) -> Error {
    Error::Storage(error.to_string())
}
fn refuse(detail: &str) -> Error {
    Error::Storage(format!("instance import refused: {detail}"))
}
fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[derive(Clone)]
struct Column {
    name: String,
    kind: String,
    pk: usize,
}
fn columns(conn: &Connection, table: &str) -> Result<Vec<Column>, Error> {
    conn.prepare(&format!("PRAGMA table_info({})", ident(table)))
        .map_err(storage)?
        .query_map([], |row| {
            Ok(Column {
                name: row.get(1)?,
                kind: row.get(2)?,
                pk: row.get(5)?,
            })
        })
        .map_err(storage)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(storage)
}

struct Table {
    name: String,
    columns: Vec<Column>,
    rows: Vec<Vec<Value>>,
}
fn table(conn: &Connection, name: &str) -> Result<Table, Error> {
    let columns = columns(conn, name)?;
    if columns.is_empty() {
        return Err(refuse(
            "source must be normalized with the current Store schema",
        ));
    }
    let mut pk: Vec<_> = columns.iter().filter(|column| column.pk > 0).collect();
    pk.sort_by_key(|column| column.pk);
    if pk.is_empty() {
        return Err(refuse("an import table has no primary key"));
    }
    let names = columns
        .iter()
        .map(|column| ident(&column.name))
        .collect::<Vec<_>>()
        .join(",");
    let order = pk
        .iter()
        .map(|column| ident(&column.name))
        .collect::<Vec<_>>()
        .join(",");
    let rows = conn
        .prepare(&format!(
            "SELECT {names} FROM {} ORDER BY {order}",
            ident(name)
        ))
        .map_err(storage)?
        .query_map([], |row| {
            (0..columns.len())
                .map(|index| row.get::<_, Value>(index))
                .collect::<Result<Vec<_>, _>>()
        })
        .map_err(storage)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(storage)?;
    Ok(Table {
        name: name.into(),
        columns,
        rows,
    })
}
fn position(table: &Table, name: &str) -> Option<usize> {
    table.columns.iter().position(|column| column.name == name)
}
fn text(value: &Value) -> Result<&str, Error> {
    if let Value::Text(value) = value {
        Ok(value)
    } else {
        Err(refuse("unexpected text column shape"))
    }
}
fn packed(value: &Value) -> serde_json::Value {
    match value {
        Value::Null => serde_json::json!(["null"]),
        Value::Integer(value) => serde_json::json!(["integer", value]),
        Value::Real(value) => serde_json::json!(["real", value]),
        Value::Text(value) => serde_json::json!(["text", value]),
        Value::Blob(value) => serde_json::json!(["blob", hex::encode(value)]),
    }
}
fn fingerprint(tables: &[Table]) -> String {
    let mut digest = Sha256::new();
    for table in tables {
        let header = serde_json::to_vec(&(
            table.name.as_str(),
            table
                .columns
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>(),
        ))
        .expect("serializable column names");
        digest.update((header.len() as u64).to_le_bytes());
        digest.update(header);
        for row in &table.rows {
            let body = serde_json::to_vec(&row.iter().map(packed).collect::<Vec<_>>())
                .expect("serializable SQLite values");
            digest.update((body.len() as u64).to_le_bytes());
            digest.update(body);
        }
    }
    hex::encode(digest.finalize())
}
fn reencrypt(
    row: &mut [Value],
    table: &Table,
    source: &SecretStore,
    destination: &SecretStore,
) -> Result<bool, Error> {
    let (aad, payload) = match table.name.as_str() {
        "secrets" | "secret_versions" => ("name", "data"),
        "credential_requests" => ("id", "proposed_data"),
        _ => return Ok(false),
    };
    let aad =
        text(&row[position(table, aad).ok_or_else(|| refuse("encrypted table shape differs"))?])?
            .to_string();
    let payload =
        position(table, payload).ok_or_else(|| refuse("encrypted table shape differs"))?;
    if let Value::Blob(bytes) = &row[payload] {
        let clear = Zeroizing::new(source.decrypt_with_aad(&aad, bytes)?);
        row[payload] = Value::Blob(destination.encrypt_with_aad(&aad, &clear)?);
        return Ok(true);
    }
    if row[payload] != Value::Null {
        return Err(refuse("encrypted payload is not a blob"));
    }
    Ok(false)
}

impl Store {
    /// SQLite's backup API includes committed WAL pages without modifying the source.
    /// The output must not exist; callers keep it in a private staging directory.
    pub fn snapshot_database(source: &Path, output: &Path) -> Result<String, Error> {
        if !source.is_file() || output.exists() {
            return Err(refuse("snapshot source must exist and output must be new"));
        }
        let source = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(storage)?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        drop(
            options
                .open(output)
                .map_err(|error| Error::Storage(error.to_string()))?,
        );
        let mut destination = Connection::open(output).map_err(storage)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(output, std::fs::Permissions::from_mode(0o600))
                .map_err(|error| Error::Storage(error.to_string()))?;
        }
        rusqlite::backup::Backup::new(&source, &mut destination)
            .map_err(storage)?
            .run_to_completion(256, Duration::from_millis(10), None)
            .map_err(storage)?;
        drop(destination);
        let bytes = std::fs::read(output).map_err(|error| Error::Storage(error.to_string()))?;
        Ok(hex::encode(Sha256::digest(bytes)))
    }

    /// Import a normalized private snapshot into an offline destination, atomically.
    /// A label may be imported once: an identical replay is a no-op, a changed
    /// source is refused. Conflicting keys roll back the whole import. No value
    /// appears in the public receipt or error messages.
    pub async fn import_instance(
        &self,
        source: &Store,
        label: &str,
    ) -> Result<InstanceImportReport, Error> {
        if label.is_empty()
            || label.len() > 64
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        {
            return Err(refuse(
                "source label must be 1-64 letters, digits, hyphens or underscores",
            ));
        }
        let tables = {
            let conn = source
                .conn
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let names = conn.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name").map_err(storage)?
                .query_map([], |row| row.get::<_, String>(0)).map_err(storage)?.collect::<Result<Vec<_>, _>>().map_err(storage)?;
            let known: BTreeSet<_> = DATA_TABLES
                .iter()
                .chain(RUNTIME_TABLES)
                .copied()
                .chain(["instance_imports"])
                .collect();
            for name in names {
                if !known.contains(name.as_str()) {
                    return Err(refuse(
                        "source has an unrecognized table; extend the migration explicitly",
                    ));
                }
            }
            if conn
                .query_row("SELECT COUNT(*) FROM instance_imports", [], |row| {
                    row.get::<_, i64>(0)
                })
                .map_err(storage)?
                != 0
            {
                return Err(refuse(
                    "nested instance imports need explicit reconciliation",
                ));
            }
            if conn
                .query_row("SELECT COUNT(*) FROM leases", [], |row| {
                    row.get::<_, i64>(0)
                })
                .map_err(storage)?
                != 0
            {
                return Err(refuse("source still has live leases"));
            }
            if conn.query_row("SELECT COUNT(*) FROM work_items WHERE status IN ('queued','ready','leased','running','verifying')", [], |row| row.get::<_, i64>(0)).map_err(storage)? != 0 {
                return Err(refuse("source still has executable work; drain it before consolidation"));
            }
            if conn
                .query_row(
                    "SELECT COUNT(*) FROM payment_requests WHERE status IN ('authorized','paying')",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(storage)?
                != 0
            {
                return Err(refuse("source has an active payment authorization"));
            }
            DATA_TABLES
                .iter()
                .chain(RUNTIME_TABLES)
                .map(|name| table(&conn, name))
                .collect::<Result<Vec<_>, _>>()?
        };
        let source_secrets = source.secrets();
        let destination_secrets = self.secrets();
        let label = label.to_string();
        with_conn(&self.conn, move |conn| {
            let fingerprint = fingerprint(&tables);
            if let Some((old, report)) = conn.query_row("SELECT fingerprint,report FROM instance_imports WHERE source=?1", [&label], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?))).optional().map_err(storage)? {
                if old != fingerprint { return Err(refuse("source label already imported with different state")); }
                return serde_json::from_str(&report).map_err(|_| refuse("stored import receipt is unreadable"));
            }
            if conn.query_row("SELECT COUNT(*) FROM leases", [], |row| row.get::<_,i64>(0)).map_err(storage)? != 0 {
                return Err(refuse("destination still has live leases"));
            }
            let tx = conn.unchecked_transaction().map_err(storage)?;
            tx.execute_batch("PRAGMA defer_foreign_keys=ON").map_err(storage)?;
            let now = chrono::Utc::now().to_rfc3339();
            let mut report = InstanceImportReport { source: label.clone(), fingerprint, imported_at: now.clone(), inserted: BTreeMap::new(), reused: BTreeMap::new(), retained_in_snapshot: BTreeMap::new(), integer_id_offsets: BTreeMap::new(), previously_enabled_schedules: Vec::new(), retired_pending_notices: 0, reencrypted_values: 0 };
            for table in &tables {
                if RUNTIME_TABLES.contains(&table.name.as_str()) { report.retained_in_snapshot.insert(table.name.clone(), table.rows.len()); continue; }
                let target_columns = columns(&tx, &table.name)?;
                let left: BTreeSet<_> = table.columns.iter().map(|column| column.name.as_str()).collect();
                let right: BTreeSet<_> = target_columns.iter().map(|column| column.name.as_str()).collect();
                if left != right { return Err(refuse("normalized source and destination columns differ")); }
                let numeric_pk: Vec<_> = table.columns.iter().enumerate().filter(|(_,column)| column.pk > 0 && column.kind.eq_ignore_ascii_case("INTEGER")).collect();
                let pk_count = table.columns.iter().filter(|column| column.pk > 0).count();
                let offset = if pk_count == 1 && numeric_pk.len() == 1 && !table.rows.is_empty() {
                    let (index,column) = numeric_pk[0];
                    let low = tx.query_row(&format!("SELECT COALESCE(MIN({}),0) FROM {}", ident(&column.name),ident(&table.name)), [], |row| row.get::<_,i64>(0)).map_err(storage)?.min(0);
                    let high = table.rows.iter().map(|row| match row[index] { Value::Integer(value) => Ok(value), _ => Err(refuse("integer primary key has another shape")) }).collect::<Result<Vec<_>,_>>()?.into_iter().max().unwrap_or(0);
                    let offset = low.checked_sub(high).and_then(|value| value.checked_sub(1)).ok_or_else(|| refuse("integer ID range exhausted"))?;
                    report.integer_id_offsets.insert(table.name.clone(),offset); Some((index,offset))
                } else { None };
                let names = table.columns.iter().map(|column| ident(&column.name)).collect::<Vec<_>>().join(",");
                let placeholders = (1..=table.columns.len()).map(|index| format!("?{index}")).collect::<Vec<_>>().join(",");
                let pk: Vec<_> = table.columns.iter().enumerate().filter(|(_,column)| column.pk > 0).collect();
                let predicate = pk.iter().enumerate().map(|(index,(_,column))| format!("{} IS ?{}",ident(&column.name),index+1)).collect::<Vec<_>>().join(" AND ");
                let mut inserted = 0; let mut reused = 0;
                for original in &table.rows {
                    let mut row = original.clone();
                    if let Some((index,offset)) = offset { if let Value::Integer(value) = row[index] { row[index] = Value::Integer(value.checked_add(offset).ok_or_else(|| refuse("integer ID range exhausted"))?); } }
                    if table.name == "scheduled_jobs" {
                        let enabled = position(table,"enabled").expect("current job schema");
                        if row[enabled] == Value::Integer(1) { report.previously_enabled_schedules.push(text(&row[position(table,"id").expect("current job schema")])?.into()); }
                        row[enabled] = Value::Integer(0);
                    }
                    if table.name == "work_outbox" {
                        let delivered = position(table,"delivered_at").expect("current outbox schema");
                        if row[delivered] == Value::Null { report.retired_pending_notices += 1; }
                        row[position(table,"retired_at").expect("current outbox schema")] = Value::Text(now.clone());
                    }
                    if table.name == "proposal_evidence" && text(&row[position(table,"source").expect("current evidence schema")])? == "work_event" {
                        let index = position(table,"ref").expect("current evidence schema");
                        let old = text(&row[index])?.parse::<i64>().map_err(|_| refuse("non-numeric event reference requires explicit migration"))?;
                        let offset = *report.integer_id_offsets.get("work_item_events").ok_or_else(|| refuse("event reference has no imported event table"))?;
                        row[index] = Value::Text(old.checked_add(offset).ok_or_else(|| refuse("event reference range exhausted"))?.to_string());
                    }
                    if reencrypt(&mut row,table,&source_secrets,&destination_secrets)? { report.reencrypted_values += 1; }
                    let key = pk.iter().map(|(index,_)| row[*index].clone()).collect::<Vec<_>>();
                    let existing = tx.query_row(&format!("SELECT {names} FROM {} WHERE {predicate}",ident(&table.name)),params_from_iter(key),|found| (0..table.columns.len()).map(|index| found.get::<_,Value>(index)).collect::<Result<Vec<_>,_>>()).optional().map_err(storage)?;
                    if let Some(existing) = existing {
                        if existing != row { return Err(refuse(&format!("conflicting primary key in {}; no destination state was replaced",table.name))); }
                        reused += 1;
                    } else {
                        tx.execute(&format!("INSERT INTO {} ({names}) VALUES ({placeholders})",ident(&table.name)),params_from_iter(row)).map_err(storage)?;
                        inserted += 1;
                    }
                }
                report.inserted.insert(table.name.clone(),inserted); report.reused.insert(table.name.clone(),reused);
            }
            if tx.prepare("PRAGMA foreign_key_check").map_err(storage)?.exists([]).map_err(storage)? { return Err(refuse("foreign key check failed")); }
            let json = serde_json::to_string(&report).map_err(|error| Error::Storage(error.to_string()))?;
            tx.execute("INSERT INTO instance_imports(source,fingerprint,report,imported_at) VALUES (?1,?2,?3,?4)",params![label,report.fingerprint,json,now]).map_err(storage)?;
            tx.commit().map_err(storage)?; Ok(report)
        }).await
    }
}

#[cfg(test)]
mod tests;
