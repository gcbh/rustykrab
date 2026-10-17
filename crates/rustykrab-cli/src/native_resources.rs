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
    /// Loopback ports the host half's shell may not reach: the daemon's own.
    pub private_ports: Vec<u16>,
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
        .with_private_ports(tools.private_ports.clone())
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
}
#[async_trait]
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
        let host =
            rustykrab_control::capability_proof::EffectiveRequirements::from_item(item).uses_host();
        if host {
            self.host.accepts(item)
        } else {
            self.native.accepts(item)
        }
    }
    fn access_profile(&self) -> Value {
        serde_json::json!({"native": self.native.access_profile(), "host": self.host.access_profile()})
    }
    fn probe_scope(
        &self,
        context: &rustykrab_control::capability_proof::ProbeContext,
    ) -> Option<rustykrab_control::capability_proof::ProbeScope> {
        use rustykrab_control::capability_proof::{
            binding_digest, tool_ceiling_digest, ExecutionPath, ProbeScope,
        };
        let host = context.requirements.uses_host();
        let ceiling_digest = if host {
            self.host.probe_scope(context)?.ceiling_digest
        } else {
            tool_ceiling_digest(&[])
        };
        Some(ProbeScope {
            worker: self.name().into(),
            path: if host {
                ExecutionPath::Host
            } else {
                ExecutionPath::Native
            },
            policy_digest: binding_digest(&self.access_profile()),
            ceiling_digest,
        })
    }
    async fn probe(
        &self,
        request: &rustykrab_control::capability_proof::ProbeRequest,
    ) -> rustykrab_control::capability_proof::OperationProbe {
        use rustykrab_control::capability_proof::{probe_through, ExecutionPath, ProbeOutcome};
        if !request.valid() || self.probe_scope(&request.context).as_ref() != Some(&request.scope) {
            return request.expected.observation(
                ProbeOutcome::Unobserved,
                "effective native binding changed since probe selection",
            );
        }
        if request.scope.path != ExecutionPath::Host {
            return request.expected.observation(
                ProbeOutcome::Unobserved,
                "native CLI execution boundary has no registered observation producer",
            );
        }
        probe_through(self, &self.host, request).await
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
        if rustykrab_control::capability_proof::EffectiveRequirements::from_brief(&brief)
            .uses_host()
        {
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
                    private_ports: Vec::new(),
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
                    private_ports: Vec::new(),
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
    fn context() -> rustykrab_control::capability_proof::ProbeContext {
        use rustykrab_control::capability_proof::*;
        ProbeContext {
            requirements: EffectiveRequirements {
                kind: rustykrab_core::work::WorkKind::Task,
                required_tools: vec![],
                required_mcp_servers: vec![],
                writable_resources: vec!["repo:/fixture".into()],
                read_only_review: false,
                workspace: Some(WorkspaceIdentity {
                    repo: "/fixture".into(),
                    base: "fixture-base".into(),
                }),
            },
            bound_dir: None,
        }
    }
    fn request(
        worker: &NativeWorker,
        context: rustykrab_control::capability_proof::ProbeContext,
    ) -> rustykrab_control::capability_proof::ProbeRequest {
        use rustykrab_control::capability_proof::*;
        let scope = worker.probe_scope(&context).unwrap();
        let action = RegisteredAction::BinaryAvailable {
            binary: "node".into(),
        };
        ProbeRequest {
            expected: ExpectedOperation {
                operation: action.operation(),
                executor: worker.name().into(),
                inputs_digest: probe_inputs_digest(&action, &scope, &context),
                generation: "fixture:1".into(),
                run: "native-probe-fixture".into(),
            },
            action,
            scope,
            context,
            deadline: std::time::Duration::from_secs(10),
        }
    }
    fn fixture_worker(
        root: &std::path::Path,
        kind: WorkerKind,
        denied: &[&str],
        model: &str,
    ) -> NativeWorker {
        let spec: rustykrab_control::registry::WorkerSpec = serde_json::from_value(serde_json::json!({
            "kind":kind, "repos":["/fixture"], "model":model,
            "require_max":kind == WorkerKind::ClaudeCode, "require_chatgpt":kind == WorkerKind::Codex,
            "denied_tools":denied,
        })).unwrap();
        let mut config = ExternalConfig::from_spec(&spec, root).unwrap();
        config.command = root.join("cli-must-stay-idle");
        NativeWorker::new(
            "fixture",
            config,
            HostTools {
                tools: vec![Arc::new(rustykrab_tools::ExecTool::new())],
                backend: Arc::new(rustykrab_tools::work_backend::StubWorkBackend::new()),
                transcripts: Arc::new(Discard),
                integrations: false,
                private_ports: Vec::new(),
            },
        )
        .unwrap()
    }
    #[tokio::test]
    async fn subscription_probes_bind_the_real_host_path_and_never_substitute_for_native() {
        use rustykrab_control::capability_proof::*;
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let cli = root.path().join("cli-must-stay-idle");
        std::fs::write(&cli, b"#!/bin/sh\ntouch \"$0.invoked\"\nexit 1\n").unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap();
        for kind in [WorkerKind::ClaudeCode, WorkerKind::Codex] {
            let worker = fixture_worker(root.path(), kind, &[], "fixture-model");
            let native = request(&worker, context());
            assert_eq!(native.scope.path, ExecutionPath::Native);
            assert_eq!(
                worker.probe(&native).await.outcome,
                ProbeOutcome::Unobserved
            );
            let mut host_context = native.context.clone();
            host_context.bound_dir = Some(root.path().to_path_buf());
            host_context.requirements.required_tools = vec!["exec".into()];
            let host = request(&worker, host_context.clone());
            assert_eq!(host.scope.path, ExecutionPath::Host);
            let receipt = worker.probe(&host).await;
            let node = std::process::Command::new("node")
                .arg("--version")
                .output()
                .is_ok_and(|o| o.status.success());
            assert_eq!(
                receipt.outcome,
                if node {
                    ProbeOutcome::Ok
                } else {
                    ProbeOutcome::Failed
                },
                "{}",
                receipt.observation
            );
            assert_eq!(
                receipt.inputs_digest, host.expected.inputs_digest,
                "outer wrapper binding is attested"
            );
            // Reusing a valid host receipt/request cannot prove a native run.
            let mut changed = host.clone();
            changed.context = native.context;
            changed.expected.inputs_digest =
                probe_inputs_digest(&changed.action, &changed.scope, &changed.context);
            assert!(changed.valid());
            assert_eq!(
                worker.probe(&changed).await.outcome,
                ProbeOutcome::Unobserved
            );
            let other_policy = fixture_worker(root.path(), kind, &[], "different-model");
            assert_eq!(
                other_policy.probe_scope(&host_context).unwrap().path,
                ExecutionPath::Host
            );
            assert_ne!(
                other_policy
                    .probe_scope(&host_context)
                    .unwrap()
                    .policy_digest,
                host.scope.policy_digest
            );
            assert_eq!(
                other_policy.probe(&host).await.outcome,
                ProbeOutcome::Unobserved
            );
            host_context.requirements.read_only_review = true;
            assert_eq!(
                worker.probe(&request(&worker, host_context)).await.outcome,
                ProbeOutcome::Unobserved
            );
        }
        assert!(
            !root.path().join("cli-must-stay-idle.invoked").exists(),
            "no probe launched a CLI/model"
        );
    }
    #[tokio::test]
    async fn subscription_host_probe_cannot_bypass_denied_shell_or_missing_workspace() {
        use rustykrab_control::capability_proof::*;
        let root = tempfile::tempdir().unwrap();
        let worker = fixture_worker(
            root.path(),
            WorkerKind::ClaudeCode,
            &["Bash(git push*)"],
            "fixture",
        );
        let mut context = context();
        context.requirements.required_tools = vec!["exec".into()];
        context.bound_dir = Some(root.path().to_path_buf());
        let probe = request(&worker, context);
        assert_eq!(probe.scope.path, ExecutionPath::Host);
        assert_eq!(worker.probe(&probe).await.outcome, ProbeOutcome::Unobserved);
        let worker = fixture_worker(root.path(), WorkerKind::Codex, &[], "fixture");
        let mut context = probe.context;
        context.bound_dir = None;
        assert_eq!(
            worker.probe(&request(&worker, context)).await.outcome,
            ProbeOutcome::Unobserved
        );
    }
}
