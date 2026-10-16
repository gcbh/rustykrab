//! One subscription resource can execute both native repository work and host
//! tools. It retains one identity, quota, concurrency limit and routing record.
use crate::integrations::IntegrationWorker;
use async_trait::async_trait;
use rustykrab_agent::{ExternalConfig, ExternalWorker, RunTranscripts};
use rustykrab_control::worker::{Brief, RunUsage, Worker, WorkerCapabilities};
use rustykrab_core::{
    model::ModelProvider,
    work::{ResultReport, WorkItem, WorkerKind},
    Error, Tool,
};
use rustykrab_tools::WorkBackend;
use serde_json::Value;
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct HostTools {
    pub tools: Vec<Arc<dyn Tool>>,
    pub backend: Arc<dyn WorkBackend>,
    pub transcripts: Arc<dyn RunTranscripts>,
    pub integrations: bool,
}

pub(crate) struct NativeWorker {
    native: ExternalWorker,
    host: IntegrationWorker,
}
impl NativeWorker {
    pub(crate) fn new(
        name: &str,
        config: ExternalConfig,
        mut tools: HostTools,
    ) -> Result<Self, String> {
        let dir = config.data_dir.join("model-turns").join(name);
        // A host tool must not route around explicit native tool restrictions.
        // Fine-grained native patterns cannot be enforced by host exec, so
        // custom Claude deny policies retain native-only shell execution.
        if config.kind == WorkerKind::ClaudeCode {
            let shell = config.denied_tools.is_empty();
            let fetch = !config
                .denied_tools
                .iter()
                .any(|t| t == "WebFetch" || t.starts_with("WebFetch("));
            tools
                .tools
                .retain(|t| (t.name() != "exec" || shell) && (t.name() != "web_fetch" || fetch));
        }
        let (provider, claude, codex): (Arc<dyn ModelProvider>, _, _) = match config.kind {
            WorkerKind::ClaudeCode => {
                let model = config.model.clone().unwrap_or_else(|| "sonnet".into());
                let mut provider = rustykrab_providers::ClaudeCliProvider::new(
                    config.command.clone(),
                    config.claude_config_dir.clone(),
                    model,
                    dir,
                    config.timeout,
                    120_000,
                );
                // Default native selection can differ from the host turn's
                // model. With no explicit model, honor every account window.
                provider.runtime = Arc::new(
                    rustykrab_providers::ClaudeMaxRuntime::new(
                        config.command.clone(),
                        config.claude_config_dir.clone(),
                    )
                    .with_model(config.model.as_deref()),
                );
                let provider = Arc::new(provider);
                (provider.clone(), Some(provider.runtime.clone()), None)
            }
            WorkerKind::Codex => {
                let provider = Arc::new(rustykrab_providers::CodexCliProvider::new(
                    config.command.clone(),
                    config.codex_home.clone(),
                    config.model.clone(),
                    dir,
                    config.timeout,
                ));
                (provider.clone(), None, Some(provider.runtime.clone()))
            }
            _ => return Err("host tools require a native subscription resource".into()),
        };
        let host = IntegrationWorker::general(
            name,
            config.model.clone(),
            provider,
            tools.tools,
            tools.backend,
            tools.transcripts,
            Arc::new(tokio::sync::Semaphore::new(1)),
            &config.repos,
            tools.integrations,
        )
        .map_err(|e| e.to_string())?
        .with_writable_files(&config.writable_files)
        .with_readable_files(&config.readable_files)
        // The host half's runs leave session logs where the native half's
        // do, so either is opened from the same evidence.
        .with_private_paths(vec![config.data_dir.clone()])
        .with_run_logs(
            config
                .data_dir
                .join(rustykrab_agent::external_worker::RUN_LOGS_DIR),
        );
        Ok(Self {
            native: ExternalWorker::new(name, config).with_subscription_runtimes(claude, codex),
            host,
        })
    }
    fn uses_host(brief: &Brief) -> bool {
        !brief.required_tools.is_empty()
            || brief
                .writable_resources
                .iter()
                .any(|r| !r.starts_with("repo:"))
    }
}
#[async_trait]
/// `probe_scope` and `probe` keep the trait's defaults (no scope,
/// `Unobserved`) on purpose: a run goes to the native CLI or to the host half
/// per brief, and a probe through the host half would not attest the native
/// CLI's boundary. Binding a probe to the path a run would take is open work.
impl Worker for NativeWorker {
    fn name(&self) -> &str {
        self.native.name()
    }
    fn kind(&self) -> WorkerKind {
        self.native.kind()
    }
    fn capabilities(&self) -> WorkerCapabilities {
        let mut caps = self.native.capabilities();
        let host = self.host.capabilities();
        caps.tools = host.tools;
        caps.mcp_servers = host.mcp_servers;
        for resource in host.writable_resources {
            if !caps.writable_resources.contains(&resource) {
                caps.writable_resources.push(resource);
            }
        }
        caps
    }
    fn accepts(&self, item: &WorkItem) -> bool {
        let host = !item.required_tools.is_empty()
            || item
                .writable_resources
                .iter()
                .any(|r| !r.starts_with("repo:"));
        if host {
            self.host.accepts(item)
        } else {
            self.native.accepts(item)
        }
    }
    fn isolates_worktree(&self) -> bool {
        self.native.isolates_worktree() && self.host.isolates_worktree()
    }

    fn concurrency(&self) -> usize {
        self.native.concurrency()
    }
    fn healthy(&self) -> bool {
        self.native.healthy()
    }
    fn unhealthy_reason(&self) -> Option<String> {
        self.native.unhealthy_reason()
    }
    fn runtime_status(&self) -> Option<Value> {
        self.native.runtime_status()
    }
    async fn refresh(&self) -> bool {
        self.native.refresh().await
    }
    fn usage(&self, run: &str) -> Option<RunUsage> {
        self.host.usage(run).or_else(|| self.native.usage(run))
    }
    fn run_log(&self, run: &str) -> Option<std::path::PathBuf> {
        self.host.run_log(run).or_else(|| self.native.run_log(run))
    }
    fn stop(&self, run: &str) {
        self.host.stop(run);
        self.native.stop(run);
    }
    async fn run(&self, brief: Brief) -> Result<ResultReport, Error> {
        if Self::uses_host(&brief) {
            self.host.run(brief).await
        } else {
            self.native.run(brief).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Discard;
    #[async_trait]
    impl RunTranscripts for Discard {
        async fn save(&self, _: &rustykrab_core::types::Conversation) -> Result<(), Error> {
            Ok(())
        }
    }
    #[test]
    fn both_native_subscriptions_advertise_general_work_without_losing_identity_or_scope() {
        let root = tempfile::tempdir().unwrap();
        for (kind, max, chatgpt) in [
            (WorkerKind::ClaudeCode, true, false),
            (WorkerKind::Codex, false, true),
        ] {
            let spec: rustykrab_control::registry::WorkerSpec = serde_json::from_value(serde_json::json!({"kind":kind,"repos":["/fixture"],"require_max":max,"require_chatgpt":chatgpt})).unwrap();
            let config = ExternalConfig::from_spec(&spec, root.path()).unwrap();
            let worker = NativeWorker::new(
                "fixture",
                config,
                HostTools {
                    tools: vec![],
                    backend: Arc::new(rustykrab_tools::work_backend::StubWorkBackend::new()),
                    transcripts: Arc::new(Discard),
                    integrations: true,
                },
            )
            .unwrap();
            assert_eq!(worker.kind(), kind);
            let caps = worker.capabilities();
            assert!(caps.tools.contains(&"work_plan".into()));
            assert!(caps.tools.contains(&"result_report".into()));
            assert_eq!(caps.repos, vec!["/fixture"]);
            assert_eq!(
                caps.writable_resources,
                vec!["repo:/fixture", "obsidian:daily-briefings"]
            );
            let mut item: WorkItem = serde_json::from_value(serde_json::json!({"id":"fixture","kind":"personal","title":"fixture","objective":"fixture","done_when":"fixture","status":{"status":"ready"},"created_at":"2026-10-12T00:00:00Z","updated_at":"2026-10-12T00:00:00Z","required_tools":["work_plan"]})).unwrap();
            assert!(worker.accepts(&item));
            item.writable_resources = vec!["repo:/unregistered".into()];
            assert!(!worker.accepts(&item));
        }
    }

    #[test]
    fn explicit_native_shell_restrictions_cannot_be_bypassed_through_host_exec() {
        let root = tempfile::tempdir().unwrap();
        for (allowed, denied, shell) in [
            (serde_json::json!([]), serde_json::json!([]), true),
            (serde_json::json!(["Read"]), serde_json::json!([]), true),
            (
                serde_json::json!(["Bash"]),
                serde_json::json!(["Bash(git push*)"]),
                false,
            ),
        ] {
            let spec: rustykrab_control::registry::WorkerSpec = serde_json::from_value(serde_json::json!({"kind":"claude_code","repos":["/fixture"],"require_max":true,"allowed_tools":allowed,"denied_tools":denied})).unwrap();
            let worker = NativeWorker::new(
                "fixture",
                ExternalConfig::from_spec(&spec, root.path()).unwrap(),
                HostTools {
                    tools: vec![Arc::new(rustykrab_tools::ExecTool::new())],
                    backend: Arc::new(rustykrab_tools::work_backend::StubWorkBackend::new()),
                    transcripts: Arc::new(Discard),
                    integrations: false,
                },
            )
            .unwrap();
            assert_eq!(
                worker.capabilities().tools.iter().any(|t| t == "exec"),
                shell
            );
            assert!(worker.capabilities().tools.iter().any(|t| t == "work_plan"));
        }
    }
}
