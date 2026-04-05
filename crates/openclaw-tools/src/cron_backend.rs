use async_trait::async_trait;
use openclaw_core::Result;
use serde_json::Value;

#[async_trait]
pub trait CronBackend: Send + Sync {
    async fn create_job(&self, schedule: &str, task: &str) -> Result<Value>;
    async fn list_jobs(&self) -> Result<Value>;
    async fn delete_job(&self, job_id: &str) -> Result<Value>;
}
