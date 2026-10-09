//! Offline memory merge: verbatim content and validity are preserved, while
//! retrieval ownership moves to the destination agent with explicit provenance.
use super::{storage_err, SqliteMemoryStorage};
use rusqlite::{params, params_from_iter, types::Value, OptionalExtension};
use rustykrab_core::{Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use uuid::Uuid;

const TABLES: &[&str] = &["memories", "chunks", "extracted_facts", "memory_links"];
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryImportReport {
    pub source: String,
    pub fingerprint: String,
    pub destination_agent: Uuid,
    pub inserted: BTreeMap<String, usize>,
    pub reused: BTreeMap<String, usize>,
}
pub(super) fn migrate(conn: &rusqlite::Connection) -> Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS memory_imports(source TEXT PRIMARY KEY,fingerprint TEXT NOT NULL,report TEXT NOT NULL)").map_err(storage_err)
}
fn refuse(detail: &str) -> Error {
    Error::Storage(format!("memory import refused: {detail}"))
}
struct Table {
    name: String,
    columns: Vec<String>,
    pk: Vec<usize>,
    rows: Vec<Vec<Value>>,
}
fn load(conn: &rusqlite::Connection, name: &str) -> Result<Table> {
    let columns = conn
        .prepare(&format!("PRAGMA table_info({name})"))
        .map_err(storage_err)?
        .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, usize>(5)?)))
        .map_err(storage_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(storage_err)?;
    let pk = columns
        .iter()
        .enumerate()
        .filter(|(_, (_, key))| *key > 0)
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    let columns = columns
        .into_iter()
        .map(|(name, _)| name)
        .collect::<Vec<_>>();
    if pk.is_empty() {
        return Err(refuse("normalized table has no primary key"));
    }
    let order = pk
        .iter()
        .map(|i| columns[*i].as_str())
        .collect::<Vec<_>>()
        .join(",");
    let rows = conn
        .prepare(&format!(
            "SELECT {} FROM {name} ORDER BY {order}",
            columns.join(",")
        ))
        .map_err(storage_err)?
        .query_map([], |r| {
            (0..columns.len())
                .map(|i| r.get::<_, Value>(i))
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .map_err(storage_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(storage_err)?;
    Ok(Table {
        name: name.into(),
        columns,
        pk,
        rows,
    })
}
impl SqliteMemoryStorage {
    /// Merge a normalized snapshot atomically. An identical label replay is a
    /// no-op; changed snapshots or conflicting IDs require explicit resolution.
    /// FTS is rebuilt in the same transaction; cached embeddings are invalidated.
    pub async fn import_instance(
        &self,
        source: &Self,
        label: &str,
        destination_agent: Uuid,
    ) -> Result<MemoryImportReport> {
        if label.is_empty()
            || label.len() > 64
            || !label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(refuse("invalid source label"));
        }
        let tables = source
            .with_conn(|conn| {
                if conn
                    .query_row("SELECT COUNT(*) FROM memory_imports", [], |r| {
                        r.get::<_, i64>(0)
                    })
                    .map_err(storage_err)?
                    != 0
                {
                    return Err(refuse("nested imports require explicit reconciliation"));
                }
                TABLES
                    .iter()
                    .map(|name| load(conn, name))
                    .collect::<Result<Vec<_>>>()
            })
            .await?;
        let label = label.to_string();
        let result=self.with_conn(move |conn| {
            let mut hash=Sha256::new();
            for table in &tables {
                hash.update(table.name.as_bytes());
                hash.update(format!("{:?}",table.columns).as_bytes());
                for row in &table.rows { hash.update(format!("{row:?}").as_bytes()); }
            }
            let fingerprint=hex::encode(hash.finalize());
            if let Some((old,json))=conn.query_row("SELECT fingerprint,report FROM memory_imports WHERE source=?1",[&label],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).optional().map_err(storage_err)? {
                let report:MemoryImportReport=serde_json::from_str(&json).map_err(|_|refuse("invalid stored receipt"))?;
                if old!=fingerprint || report.destination_agent!=destination_agent { return Err(refuse("source label already imported with different state or destination")); }
                return Ok(report);
            }
            let tx=conn.unchecked_transaction().map_err(storage_err)?;
            tx.execute_batch("PRAGMA defer_foreign_keys=ON").map_err(storage_err)?;
            let mut report=MemoryImportReport {source:label.clone(),fingerprint,destination_agent,inserted:BTreeMap::new(),reused:BTreeMap::new()};
            for table in tables {
                let target=load(&tx,&table.name)?;
                if target.columns!=table.columns { return Err(refuse("normalized source and destination columns differ")); }
                let predicate=table.pk.iter().enumerate().map(|(i,p)|format!("{} IS ?{}",table.columns[*p],i+1)).collect::<Vec<_>>().join(" AND ");
                let placeholders=(1..=table.columns.len()).map(|i|format!("?{i}")).collect::<Vec<_>>().join(",");
                let names=table.columns.join(",");
                let mut inserted=0; let mut reused=0;
                for mut row in table.rows {
                    if table.name=="memories" {
                        let agent=table.columns.iter().position(|n|n=="agent_id").unwrap();
                        let metadata=table.columns.iter().position(|n|n=="metadata").unwrap();
                        let Value::Text(ref original)=row[agent] else {return Err(refuse("invalid source agent"));};
                        let Value::Text(ref json)=row[metadata] else {return Err(refuse("invalid metadata"));};
                        let mut object:serde_json::Map<String,serde_json::Value>=serde_json::from_str(json).map_err(|_|refuse("metadata must be an object"))?;
                        if object.contains_key("instance_import") {return Err(refuse("existing import provenance requires reconciliation"));}
                        object.insert("instance_import".into(),serde_json::json!({"source":label,"original_agent_id":original}));
                        row[metadata]=Value::Text(serde_json::to_string(&object).map_err(|_|refuse("invalid metadata"))?);
                        row[agent]=Value::Text(destination_agent.to_string());
                    }
                    let key=table.pk.iter().map(|p|row[*p].clone()).collect::<Vec<_>>();
                    let existing=tx.query_row(&format!("SELECT {names} FROM {} WHERE {predicate}",table.name),params_from_iter(key),|r|(0..table.columns.len()).map(|i|r.get::<_,Value>(i)).collect::<rusqlite::Result<Vec<_>>>()).optional().map_err(storage_err)?;
                    if let Some(existing)=existing {
                        if existing!=row {return Err(refuse("conflicting memory ID; destination was not replaced"));}
                        reused+=1;
                    } else {
                        tx.execute(&format!("INSERT INTO {} ({names}) VALUES ({placeholders})",table.name),params_from_iter(row)).map_err(storage_err)?;
                        inserted+=1;
                    }
                }
                report.inserted.insert(table.name.clone(),inserted); report.reused.insert(table.name,reused);
            }
            if tx.prepare("PRAGMA foreign_key_check").map_err(storage_err)?.exists([]).map_err(storage_err)? {return Err(refuse("foreign key check failed"));}
            tx.execute_batch("DELETE FROM memories_fts; INSERT INTO memories_fts(memory_id,agent_id,content) SELECT id,agent_id,content FROM memories WHERE is_valid=1").map_err(storage_err)?;
            let json=serde_json::to_string(&report).map_err(|_|refuse("invalid report"))?;
            tx.execute("INSERT INTO memory_imports VALUES (?1,?2,?3)",params![label,report.fingerprint,json]).map_err(storage_err)?;
            tx.commit().map_err(storage_err)?; Ok(report)
        }).await;
        if result.is_ok() {
            self.embedding_cache.clear();
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::MemoryStorage;
    #[tokio::test]
    async fn import_preserves_validity_and_content_but_moves_retrieval_ownership() {
        let source = SqliteMemoryStorage::open_in_memory().unwrap();
        let target = SqliteMemoryStorage::open_in_memory().unwrap();
        let old = Uuid::new_v4();
        let new = Uuid::new_v4();
        let id = Uuid::new_v4();
        let invalid = Uuid::new_v4();
        source.with_conn(move |conn| {
            conn.execute("INSERT INTO memories(id,agent_id,content,content_hash,created_at) VALUES (?1,?2,'verbatim legacy context','hash','2026-10-01T00:00:00Z')",params![id.to_string(),old.to_string()]).map_err(storage_err)?;
            conn.execute("INSERT INTO memories(id,agent_id,content,content_hash,created_at,is_valid) VALUES (?1,?2,'invalid context','hash2','2026-10-01T00:00:00Z',0)",params![invalid.to_string(),old.to_string()]).map_err(storage_err)?; Ok(())
        }).await.unwrap();
        let report = target.import_instance(&source, "main", new).await.unwrap();
        let memories = target.list_retrievable(new).await.unwrap();
        assert_eq!(memories.len(), 1);
        assert_eq!(memories[0].content, "verbatim legacy context");
        assert_eq!(
            memories[0].metadata["instance_import"]["original_agent_id"],
            old.to_string()
        );
        assert!(target.list_retrievable(old).await.unwrap().is_empty());
        assert_eq!(target.fts_search("legacy", new, 10).await.unwrap().len(), 1);
        assert_eq!(
            report,
            target.import_instance(&source, "main", new).await.unwrap()
        );
        assert!(target.import_instance(&source, "main", old).await.is_err());
        assert!(!target.get_memory(invalid).await.unwrap().unwrap().is_valid);
    }
    #[tokio::test]
    async fn conflict_rolls_back_without_losing_destination() {
        let source = SqliteMemoryStorage::open_in_memory().unwrap();
        let target = SqliteMemoryStorage::open_in_memory().unwrap();
        let id = Uuid::new_v4().to_string();
        let agent = Uuid::new_v4();
        for (db, content) in [(&source, "old"), (&target, "current")] {
            let id = id.clone();
            let content = content.to_string();
            db.with_conn(move |c| {c.execute("INSERT INTO memories(id,agent_id,content,content_hash,created_at) VALUES (?1,?2,?3,'hash','2026-10-01T00:00:00Z')",params![id,agent.to_string(),content]).map_err(storage_err)?;Ok(())}).await.unwrap();
        }
        assert!(target
            .import_instance(&source, "main", agent)
            .await
            .is_err());
        assert_eq!(
            target
                .get_memory(Uuid::parse_str(&id).unwrap())
                .await
                .unwrap()
                .unwrap()
                .content,
            "current"
        );
        assert_eq!(
            target
                .with_conn(|c| c
                    .query_row("SELECT COUNT(*) FROM memory_imports", [], |r| r
                        .get::<_, i64>(0))
                    .map_err(storage_err))
                .await
                .unwrap(),
            0
        );
    }
}
