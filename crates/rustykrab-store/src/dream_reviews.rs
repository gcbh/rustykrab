//! Durable review state machine, independent of work-item archival.
use crate::{with_conn, Store};
use rusqlite::{params, Connection, OptionalExtension};
use rustykrab_core::{dream_review::ProjectReview, Error};
fn err(e: impl std::fmt::Display) -> Error {
    Error::Storage(e.to_string())
}
pub(crate) fn migrate(c: &Connection) -> Result<(), Error> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS dream_project_reviews (
        id TEXT PRIMARY KEY, project TEXT NOT NULL, stage TEXT NOT NULL,
        version INTEGER NOT NULL, created_at TEXT NOT NULL, data TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS idx_dream_project_reviews_created
          ON dream_project_reviews(created_at);
        CREATE UNIQUE INDEX IF NOT EXISTS idx_dream_project_reviews_live
          ON dream_project_reviews((1)) WHERE stage NOT IN ('completed','failed');",
    )
    .map_err(err)
}
impl Store {
    pub async fn dream_review_create(&self, r: &ProjectReview) -> Result<(), Error> {
        let r = r.clone();
        let data = serde_json::to_string(&r).map_err(err)?;
        let stage = serde_json::to_value(r.stage).map_err(err)?;
        with_conn(&self.conn, move |c| {
            c.execute(
                "INSERT INTO dream_project_reviews(id,project,stage,version,created_at,data)
                VALUES (?1,?2,?3,?4,?5,?6)",
                params![
                    r.id,
                    r.input.project_id,
                    stage.as_str(),
                    r.version,
                    r.created_at.to_rfc3339(),
                    data
                ],
            )
            .map_err(err)?;
            Ok(())
        })
        .await
    }
    /// CAS prevents a stale/restarted driver from replacing a newer receipt.
    pub async fn dream_review_save(&self, r: &mut ProjectReview) -> Result<(), Error> {
        let prior = r.version;
        let mut next = r.clone();
        next.version += 1;
        let data = serde_json::to_string(&next).map_err(err)?;
        let stage = serde_json::to_value(next.stage).map_err(err)?;
        let id = next.id.clone();
        with_conn(&self.conn, move |c| {
            let old: String = c
                .query_row(
                    "SELECT data FROM dream_project_reviews WHERE id=?1",
                    [&id],
                    |r| r.get(0),
                )
                .map_err(err)?;
            let old: ProjectReview = serde_json::from_str(&old).map_err(err)?;
            if old.input != next.input || old.created_at != next.created_at {
                return Err(err("frozen review input cannot be changed"));
            }
            let n = c
                .execute(
                    "UPDATE dream_project_reviews SET stage=?1,version=?2,data=?3
                WHERE id=?4 AND version=?5",
                    params![stage.as_str(), next.version, data, id, prior],
                )
                .map_err(err)?;
            if n != 1 {
                return Err(err("review receipt changed concurrently"));
            }
            Ok(())
        })
        .await?;
        r.version += 1;
        Ok(())
    }
    pub async fn dream_reviews_recent(&self, limit: usize) -> Result<Vec<ProjectReview>, Error> {
        let limit = limit.clamp(1, 1000);
        with_conn(&self.conn, move |c| {
            let mut s = c
                .prepare("SELECT data FROM dream_project_reviews ORDER BY created_at DESC LIMIT ?1")
                .map_err(err)?;
            let rows = s
                .query_map([limit], |r| r.get::<_, String>(0))
                .map_err(err)?;
            rows.map(|r| serde_json::from_str(&r.map_err(err)?).map_err(err))
                .collect()
        })
        .await
    }
    pub async fn dream_review_get(&self, id: &str) -> Result<Option<ProjectReview>, Error> {
        let id = id.to_string();
        with_conn(&self.conn, move |c| {
            let raw: Option<String> = c
                .query_row(
                    "SELECT data FROM dream_project_reviews WHERE id=?1",
                    [id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(err)?;
            raw.map(|s| serde_json::from_str(&s).map_err(err))
                .transpose()
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustykrab_core::dream_review::*;
    fn review() -> ProjectReview {
        let now = chrono::Utc::now();
        ProjectReview {
            id: uuid::Uuid::new_v4().to_string(),
            version: 0,
            input: ReviewInput {
                project_id: "project".into(),
                revision: "revision".into(),
                project: serde_json::Value::Null,
                observations: vec![],
                omissions: vec![],
                captured_at: now,
            },
            stage: ReviewStage::Preparing,
            generator_item: None,
            evaluator_item: None,
            generated: None,
            meta: None,
            filed: vec![],
            skipped: vec![],
            error: None,
            created_at: now,
            updated_at: now,
        }
    }
    #[tokio::test]
    async fn receipts_are_immutable_cas_guarded_and_recover_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), vec![3; 32]).unwrap();
        let mut r = review();
        store.dream_review_create(&r).await.unwrap();
        assert!(store.dream_review_create(&review()).await.is_err());
        let mut stale = r.clone();
        r.generator_item = Some("work".into());
        r.stage = ReviewStage::Generating;
        store.dream_review_save(&mut r).await.unwrap();
        assert!(store.dream_review_save(&mut stale).await.is_err());
        let mut changed = r.clone();
        changed.input.revision = "invented".into();
        assert!(store.dream_review_save(&mut changed).await.is_err());
        drop(store);
        let reopened = Store::open(dir.path(), vec![3; 32]).unwrap();
        assert_eq!(reopened.dream_review_get(&r.id).await.unwrap().unwrap(), r);
        r.stage = ReviewStage::Completed;
        reopened.dream_review_save(&mut r).await.unwrap();
        reopened.dream_review_create(&review()).await.unwrap();
    }
}
