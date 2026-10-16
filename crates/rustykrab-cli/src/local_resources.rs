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
    /// Retired: the model's window is its `context_window` in
    /// `models.toml`. Still accepted so an existing file loads; ignored
    /// with a warning.
    #[serde(default)]
    pub context_tokens: Option<u32>,
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
        if let Some(tokens) = self.context_tokens {
            tracing::warn!(
                context_tokens = tokens,
                model = %self.model,
                "local-resources context_tokens is no longer read; set this model's \
                 context_window in models.toml"
            );
        }
        Ok(())
    }
    /// The local workers' provider: this model's window and harness from
    /// `models.toml`, sampling deterministically and without thinking
    /// unless the model's entry says otherwise.
    pub(crate) async fn provider(&self) -> Arc<dyn ModelProvider> {
        let worker_defaults = rustykrab_providers::OllamaConfig {
            think: Some(false),
            temperature: 0.0,
            ..rustykrab_providers::OllamaConfig::default()
        };
        let config = rustykrab_agent::local_model::installed()
            .for_model(&self.model)
            .ollama_config(worker_defaults);
        Arc::new(
            rustykrab_providers::OllamaProvider::new(self.model.clone())
                .with_base_url(self.url.trim_end_matches('/').to_string())
                .with_config(config)
                .with_detected_context_window()
                .await,
        )
    }
}

/// The local coding worker's toolset, modelled on Claude Code's default
/// working tools (the owner's decision of 2026-10-13): a shell, file read,
/// edit and write, web fetch and search, and a task list. In a worktree
/// run the shell and the file tools are bound to the worktree
/// (`LocalWorker::run`). All of them are visible from the first turn:
/// together their schemas cost about two thousand tokens.
pub(crate) const CODING_TOOLS: [&str; 8] = [
    "exec",
    "read",
    "edit",
    "write",
    "web_fetch",
    "web_search",
    "todo_write",
    "todo_read",
];

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
    /// Keep each run's session log in `dir` (`LocalWorker::with_run_logs`).
    pub(crate) fn with_run_logs(mut self, dir: PathBuf) -> Self {
        self.worker = self.worker.with_run_logs(dir);
        self
    }
    /// Keep `paths` out of the worker's shell (`LocalWorker::with_private_paths`).
    pub(crate) fn with_private_paths(mut self, paths: Vec<std::path::PathBuf>) -> Self {
        self.worker = self.worker.with_private_paths(paths);
        self
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
    fn isolates_worktree(&self) -> bool {
        self.worker.isolates_worktree()
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
    fn run_log(&self, run: &str) -> Option<PathBuf> {
        self.worker.run_log(run)
    }
    fn access_profile(&self) -> Value {
        self.worker.access_profile()
    }
    fn probe_scope(
        &self,
        bound_dir: Option<PathBuf>,
    ) -> Option<rustykrab_control::capability_proof::ProbeScope> {
        self.worker.probe_scope(bound_dir)
    }
    async fn probe(
        &self,
        request: &rustykrab_control::capability_proof::ProbeRequest,
    ) -> rustykrab_control::capability_proof::OperationProbe {
        self.worker.probe(request).await
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
    fn config(url: &str) -> Config {
        Config {
            model: "qwen3.8:27b-mlx".into(),
            url: url.into(),
            context_tokens: None,
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
            assert!(config(url).validate().is_err(), "{url}");
        }
        for url in [
            "http://127.0.0.1:11434",
            "http://localhost:11434",
            "http://[::1]:11434",
        ] {
            assert!(config(url).validate().is_ok(), "{url}");
        }
    }
    #[test]
    fn requires_model_identity_and_still_loads_a_retired_context_tokens() {
        let mut retired = config("http://127.0.0.1:11434");
        retired.context_tokens = Some(32768);
        assert!(retired.validate().is_ok(), "an existing file keeps loading");
        let parsed: Config = serde_json::from_str(
            r#"{"model":"m","url":"http://127.0.0.1:11434","context_tokens":32768,"repositories":[]}"#,
        )
        .unwrap();
        assert_eq!(parsed.context_tokens, Some(32768));
        let mut c = config("http://127.0.0.1:11434");
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
    /// The local coding worker's toolset is a shell plus the file, web and
    /// task-list tools the host has, not the shell alone.
    /// The production wrapper hands probes to its local worker: a scope, an
    /// observation from the local worker itself (not the trait's
    /// "cannot observe" default), and an access profile that is more than
    /// its advertised capabilities.
    #[tokio::test]
    async fn local_coding_forwards_probes_to_its_local_worker() {
        use rustykrab_control::capability_proof::{
            probe_inputs_digest, ExpectedOperation, ProbeOutcome, ProbeRequest, ProbeScope,
            RegisteredAction,
        };
        let mut c = config("http://127.0.0.1:11434");
        c.repositories = vec![PathBuf::from("/fixture")];
        let host: Vec<Arc<dyn Tool>> = vec![Arc::new(rustykrab_tools::ExecTool::new())];
        let worker = CodingWorker::new(
            &c,
            Arc::new(NoModel),
            host,
            Arc::new(rustykrab_tools::work_backend::StubWorkBackend::new()),
            Arc::new(Discard),
            Arc::new(Semaphore::new(1)),
        );
        assert_ne!(
            worker.access_profile(),
            serde_json::to_value(worker.capabilities()).unwrap(),
            "the wrapper's access profile is its local worker's, not the advertised default"
        );
        let node = RegisteredAction::BinaryAvailable {
            binary: "node".into(),
        };
        let request = |scope: ProbeScope| ProbeRequest {
            expected: ExpectedOperation {
                operation: node.operation(),
                executor: worker.name().to_string(),
                inputs_digest: probe_inputs_digest(&node, &scope),
                generation: "fixture:1".into(),
                run: "probe-fixture".into(),
            },
            action: node.clone(),
            scope,
            deadline: std::time::Duration::from_secs(10),
        };

        // Without a bound directory the repository shell has no exec, so the
        // probe cannot run and says so.
        let unbound = worker
            .probe_scope(None)
            .expect("the wrapper has its local worker's probe scope");
        assert_eq!(unbound.worker, worker.name());
        let probe = worker.probe(&request(unbound)).await;
        assert_eq!(
            probe.outcome,
            ProbeOutcome::Unobserved,
            "{}",
            probe.observation
        );
        assert_eq!(
            probe.observation,
            "registered tool is absent from this worker"
        );

        // Bound to a real directory, the local worker runs `node --version`
        // there through its own exec: Ok where the host has node, Failed
        // where it does not, never unobserved.
        let dir = tempfile::tempdir().unwrap();
        let bound = worker
            .probe_scope(Some(dir.path().to_path_buf()))
            .expect("the wrapper has its local worker's probe scope");
        assert_eq!(bound.bound_dir.as_deref(), Some(dir.path()));
        let probe = worker.probe(&request(bound)).await;
        let host_has_node = std::process::Command::new("node")
            .arg("--version")
            .current_dir(dir.path())
            .output()
            .is_ok_and(|out| out.status.success());
        let expected = if host_has_node {
            ProbeOutcome::Ok
        } else {
            ProbeOutcome::Failed
        };
        assert_eq!(probe.outcome, expected, "{}", probe.observation);
    }

    #[test]
    fn local_coding_gets_file_web_and_task_tools_beside_its_shell() {
        let mut c = config("http://127.0.0.1:11434");
        c.repositories = vec![PathBuf::from("/fixture")];
        let mut host: Vec<Arc<dyn Tool>> = vec![
            Arc::new(rustykrab_tools::ExecTool::new()),
            Arc::new(rustykrab_tools::ReadTool::new()),
            Arc::new(rustykrab_tools::EditTool::new()),
            Arc::new(rustykrab_tools::WriteTool::new()),
        ];
        host.extend(rustykrab_agent::todo_tools());
        let worker = CodingWorker::new(
            &c,
            Arc::new(NoModel),
            host,
            Arc::new(rustykrab_tools::work_backend::StubWorkBackend::new()),
            Arc::new(Discard),
            Arc::new(Semaphore::new(1)),
        );
        let tools = worker.capabilities().tools;
        for tool in ["exec", "read", "edit", "write", "todo_write", "todo_read"] {
            assert!(tools.iter().any(|t| t == tool), "{tool}: {tools:?}");
        }
        // Named in the toolset, so they are granted wherever the host has
        // them; this host has none, and a tool it lacks is not advertised.
        assert!(CODING_TOOLS.contains(&"web_fetch") && CODING_TOOLS.contains(&"web_search"));
        assert!(!tools.iter().any(|t| t == "web_fetch"), "{tools:?}");
    }

    /// The wrapper names the session log its inner worker keeps.
    #[test]
    fn local_coding_names_the_session_log_its_worker_keeps() {
        let mut c = config("http://127.0.0.1:11434");
        c.repositories = vec![PathBuf::from("/fixture")];
        let dir = tempfile::tempdir().unwrap();
        let worker = CodingWorker::new(
            &c,
            Arc::new(NoModel),
            vec![],
            Arc::new(rustykrab_tools::work_backend::StubWorkBackend::new()),
            Arc::new(Discard),
            Arc::new(Semaphore::new(1)),
        )
        .with_run_logs(dir.path().to_path_buf());
        assert_eq!(worker.run_log("run-1"), None, "no log before a run");
        let log = dir.path().join("run-1.jsonl");
        std::fs::write(&log, "{}\n").unwrap();
        assert_eq!(worker.run_log("run-1"), Some(log));
    }

    #[test]
    fn local_coding_advertises_actual_model_and_refuses_other_resources() {
        let mut c = config("http://127.0.0.1:11434");
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
