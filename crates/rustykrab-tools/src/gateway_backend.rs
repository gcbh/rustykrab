use async_trait::async_trait;
use rustykrab_core::Result;
use serde_json::Value;

#[async_trait]
pub trait GatewayBackend: Send + Sync {
    async fn status(&self) -> Result<Value>;
    async fn health(&self) -> Result<Value>;
    async fn get_config(&self, key: Option<&str>) -> Result<Value>;
    async fn set_config(&self, key: &str, value: &str) -> Result<Value>;
}
