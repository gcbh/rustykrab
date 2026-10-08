use async_trait::async_trait;
use rustykrab_core::Result;
use serde_json::Value;

#[async_trait]
pub trait CronBackend: Send + Sync {
    /// `timezone` is the IANA zone the caller wrote `schedule` in.
    /// `None` means "the operator's configured zone" — the common case,
    /// since the model is not told what zone the user lives in.
    ///
    /// `allow_duplicate` opts out of the check that refuses a second
    /// recurring job delivering the same task to the same destination.
    #[allow(clippy::too_many_arguments)]
    async fn create_job(
        &self,
        schedule: &str,
        task: &str,
        channel: Option<&str>,
        chat_id: Option<&str>,
        thread_id: Option<&str>,
        timezone: Option<&str>,
        allow_duplicate: bool,
    ) -> Result<Value>;
    /// Optional resource requirements; adapters without a durable controller
    /// refuse these instead of silently executing on the wrong resource.
    #[allow(clippy::too_many_arguments)]
    async fn create_managed_job(
        &self,
        schedule: &str,
        task: &str,
        channel: Option<&str>,
        chat_id: Option<&str>,
        thread_id: Option<&str>,
        timezone: Option<&str>,
        allow_duplicate: bool,
        execution: Option<rustykrab_core::work::CronExecution>,
    ) -> Result<Value> {
        if execution.is_some() {
            return Err(rustykrab_core::Error::Config(
                "managed scheduled execution is unavailable".into(),
            ));
        }
        self.create_job(
            schedule,
            task,
            channel,
            chat_id,
            thread_id,
            timezone,
            allow_duplicate,
        )
        .await
    }
    async fn list_jobs(&self) -> Result<Value>;
    async fn delete_job(&self, job_id: &str) -> Result<Value>;
    async fn list_runs(&self, job_id: &str, limit: u32) -> Result<Value>;
}
