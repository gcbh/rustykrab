//! Operator-owned local inference resources, independent of the chat provider.
use async_trait::async_trait;
use rustykrab_agent::RunTranscripts;
use rustykrab_control::worker::{Brief, RunUsage, Worker, WorkerCapabilities};
use rustykrab_core::model::ModelProvider;
#[cfg(test)]
use rustykrab_core::work::WorkKind;
use rustykrab_core::work::{ResultReport, WorkItem, WorkerKind};
#[cfg(test)]
use rustykrab_core::Error;
use rustykrab_core::{Result, Tool};
use serde::Deserialize;
use serde_json::Value;
use std::{path::PathBuf, sync::Arc};
use tokio::sync::Semaphore;

const CONFIG_ENV: &str = "RUSTYKRAB_LOCAL_RESOURCES";
pub(crate) const CODE_NAME: &str = "qwen-code";
pub(crate) const INTEGRATION_NAME: &str = "qwen-integrations";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub model: String,
    pub url: String,
    pub context_tokens: u32,
    pub repositories: Vec<PathBuf>,
}
impl Config {
    pub(crate) fn from_env() -> anyhow::Result<Option<Self>> {
        let Some(path) = std::env::var_os(CONFIG_ENV) else {
            return Ok(None);
        };
        let mut config: Self = serde_json::from_slice(&std::fs::read(path)?)?;
        config.validate()?;
        for repo in &mut config.repositories {
            anyhow::ensure!(
                repo.is_absolute() && repo.join(".git").exists(),
                "local resource repository must be an absolute git checkout"
            );
            *repo = repo.canonicalize()?;
        }
        Ok(Some(config))
    }
    fn validate(&self) -> anyhow::Result<()> {
        let url = reqwest::Url::parse(&self.url)?;
        anyhow::ensure!(
            url.scheme() == "http"
                && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))
                && url.port().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.path() == "/"
                && url.query().is_none()
                && url.fragment().is_none(),
            "local inference URL must be an explicit loopback HTTP port"
        );
        anyhow::ensure!(!self.model.trim().is_empty(), "local model must be named");
        anyhow::ensure!(
            (4096..=131072).contains(&self.context_tokens),
            "local context must be between 4096 and 131072 tokens"
        );
        Ok(())
    }
    pub(crate) async fn provider(&self) -> Arc<dyn ModelProvider> {
        Arc::new(
            rustykrab_providers::OllamaProvider::new(self.model.clone())
                .with_base_url(self.url.trim_end_matches('/').to_string())
                .with_config(rustykrab_providers::OllamaConfig {
                    num_ctx: Some(self.context_tokens),
                    think: Some(false),
                    temperature: 0.0,
                    keep_alive: Some("30m".into()),
                    top_p: 0.9,
                    num_predict: 4096,
                })
                .with_detected_context_window()
                .await,
        )
    }
}

pub(crate) struct CodingWorker {
    worker: crate::integrations::IntegrationWorker,
}
impl CodingWorker {
    pub(crate) fn new(
        config: &Config,
        provider: Arc<dyn ModelProvider>,
        tools: Vec<Arc<dyn Tool>>,
        backend: Arc<dyn rustykrab_tools::WorkBackend>,
        transcripts: Arc<dyn RunTranscripts>,
        slot: Arc<Semaphore>,
    ) -> Self {
        let repositories: Vec<String> = config
            .repositories
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        Self {
            worker: crate::integrations::IntegrationWorker::general(
                CODE_NAME,
                Some(config.model.clone()),
                provider,
                tools,
                backend,
                transcripts,
                slot,
                &repositories,
                crate::integrations::enabled(),
            )
            .expect("local resource provider is Ollama"),
        }
    }
}
#[async_trait]
impl Worker for CodingWorker {
    fn name(&self) -> &str {
        self.worker.name()
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::Local
    }
    fn capabilities(&self) -> WorkerCapabilities {
        self.worker.capabilities()
    }
    fn accepts(&self, item: &WorkItem) -> bool {
        self.worker.accepts(item)
    }
    fn healthy(&self) -> bool {
        self.worker.healthy()
    }
    fn unhealthy_reason(&self) -> Option<String> {
        self.worker.unhealthy_reason()
    }
    fn runtime_status(&self) -> Option<Value> {
        self.worker.runtime_status()
    }
    fn usage(&self, run: &str) -> Option<RunUsage> {
        self.worker.usage(run)
    }
    fn stop(&self, run: &str) {
        self.worker.stop(run)
    }
    async fn refresh(&self) -> bool {
        self.worker.refresh().await
    }
    async fn run(&self, brief: Brief) -> Result<ResultReport> {
        self.worker.run(brief).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config(url: &str, context_tokens: u32) -> Config {
        Config {
            model: "qwen3.8:27b-mlx".into(),
            url: url.into(),
            context_tokens,
            repositories: vec![],
        }
    }
    #[test]
    fn rejects_remote_or_credentialed_local_endpoints() {
        for url in [
            "https://127.0.0.1:11434",
            "http://example.com:11434",
            "http://user:password@127.0.0.1:11434",
            "http://127.0.0.1",
            "http://127.0.0.1:11434/api/chat",
            "http://127.0.0.1:11434/?secret=x",
        ] {
            assert!(config(url, 32768).validate().is_err(), "{url}");
        }
        for url in [
            "http://127.0.0.1:11434",
            "http://localhost:11434",
            "http://[::1]:11434",
        ] {
            assert!(config(url, 32768).validate().is_ok(), "{url}");
        }
    }
    #[test]
    fn bounds_local_context_and_requires_model_identity() {
        for size in [0, 4095, 131073] {
            assert!(config("http://127.0.0.1:11434", size).validate().is_err());
        }
        let mut c = config("http://127.0.0.1:11434", 32768);
        c.model.clear();
        assert!(c.validate().is_err());
    }
    struct NoModel;
    #[async_trait]
    impl ModelProvider for NoModel {
        fn name(&self) -> &str {
            "ollama"
        }
        async fn chat(
            &self,
            _: &[rustykrab_core::types::Message],
            _: &[rustykrab_core::types::ToolSchema],
        ) -> Result<rustykrab_core::model::ModelResponse> {
            Err(Error::ModelProvider("not called".into()))
        }
    }
    struct Discard;
    #[async_trait]
    impl RunTranscripts for Discard {
        async fn save(&self, _: &rustykrab_core::types::Conversation) -> Result<()> {
            Ok(())
        }
    }
    #[test]
    fn local_coding_advertises_actual_model_and_refuses_other_resources() {
        let mut c = config("http://127.0.0.1:11434", 32768);
        c.repositories = vec![PathBuf::from("/fixture")];
        let worker = CodingWorker::new(
            &c,
            Arc::new(NoModel),
            vec![],
            Arc::new(rustykrab_tools::work_backend::StubWorkBackend::new()),
            Arc::new(Discard),
            Arc::new(Semaphore::new(1)),
        );
        assert_eq!(worker.capabilities().models, vec!["qwen3.8:27b-mlx"]);
        assert_eq!(
            worker.capabilities().writable_resources,
            vec!["repo:/fixture"]
        );
        let mut item: WorkItem=serde_json::from_value(serde_json::json!({"id":"fixture","kind":"code","title":"fixture","objective":"fixture","done_when":"fixture","status":{"status":"ready"},"created_at":"2026-10-12T00:00:00Z","updated_at":"2026-10-12T00:00:00Z","writable_resources":["repo:/fixture"]})).unwrap();
        assert!(worker.accepts(&item));
        for resources in [
            vec!["repo:/elsewhere".into()],
            vec!["repo:/fixture".into(), "obsidian:daily-briefings".into()],
        ] {
            item.writable_resources = resources;
            assert!(!worker.accepts(&item));
        }
        item.writable_resources = vec![];
        assert!(
            worker.accepts(&item),
            "general analysis needs no repository scope"
        );
        item.writable_resources = vec!["repo:/fixture".into()];
        item.kind = WorkKind::Personal;
        assert!(
            worker.accepts(&item),
            "labels do not restrict repository execution"
        );
    }
}
