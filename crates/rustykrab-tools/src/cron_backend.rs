use async_trait::async_trait;
use rustykrab_core::Result;
use serde_json::Value;

#[async_trait]
pub trait CronBackend: Send + Sync {
    /// `allow_duplicate` opts out of the check that refuses a second
    /// recurring job delivering the same task to the same destination.
    async fn create_job(
        &self,
        schedule: &str,
        task: &str,
        channel: Option<&str>,
        chat_id: Option<&str>,
        thread_id: Option<&str>,
        allow_duplicate: bool,
    ) -> Result<Value>;
    async fn list_jobs(&self) -> Result<Value>;
    async fn delete_job(&self, job_id: &str) -> Result<Value>;
    async fn list_runs(&self, job_id: &str, limit: u32) -> Result<Value>;
}
