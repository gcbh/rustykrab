//! Work-manager composition and allowlisted local service execution.
//! The polling task observes services; recovery is filed through the controller.
use async_trait::async_trait;
use chrono::{TimeDelta, Utc};
use rustykrab_control::{
    handle::{ControlHandle, LockState},
    registry::WorkerRegistry,
    worker::{Brief, Worker, WorkerCapabilities, COMMAND_RUN},
    Provenance,
};
use rustykrab_core::{
    types::ToolSchema,
    work::{ArtifactRef, PlanOutcome, ResultReport, WorkItem, WorkerKind},
    Error, Result, Tool,
};
use rustykrab_gateway::resources::{
    ResourceObserver, ServiceAction, ServiceObservation, SERVICE_ACTION, SERVICE_TOOL,
};
use rustykrab_store::Store;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

pub(crate) fn manager_enabled() -> bool {
    std::env::var("RUSTYKRAB_WORK_MANAGER").is_ok_and(|v| matches!(v.as_str(), "1" | "true" | "on"))
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServiceSpec {
    id: String,
    role: String,
    health_url: String,
    binary: PathBuf,
    launchd_label: String,
    plist: PathBuf,
    #[serde(default)]
    ensure_running: bool,
}
fn validate(specs: &[ServiceSpec], manager_port: u16) -> anyhow::Result<()> {
    anyhow::ensure!(specs.len() <= 32, "at most 32 service resources");
    let mut names = HashSet::new();
    for s in specs {
        anyhow::ensure!(
            !s.id.is_empty()
                && s.id.len() <= 64
                && s.id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c))
                && names.insert(s.id.clone()),
            "invalid or duplicate service id"
        );
        anyhow::ensure!(
            !s.launchd_label.is_empty()
                && s.launchd_label
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c)),
            "invalid launchd label"
        );
        let url = reqwest::Url::parse(&s.health_url)?;
        anyhow::ensure!(
            url.scheme() == "http"
                && matches!(url.host_str(), Some("127.0.0.1" | "[::1]"))
                && url.port().is_some_and(|p| p != manager_port)
                && url.path() == "/api/health"
                && url.query().is_none()
                && url.fragment().is_none()
                && url.username().is_empty()
                && url.password().is_none(),
            "service health must be a loopback /api/health URL on another port"
        );
        anyhow::ensure!(
            s.binary.is_absolute()
                && s.plist.is_absolute()
                && s.binary.is_file()
                && s.plist.is_file(),
            "service binary and plist must be existing absolute files"
        );
    }
    Ok(())
}

pub(crate) struct Overseer {
    processes: Arc<dyn crate::update_cmd::apply::Processes>,
    specs: Vec<ServiceSpec>,
    #[cfg(test)]
    launchctl_path: PathBuf,
    observations: RwLock<Vec<ServiceObservation>>,
    client: reqwest::Client,
    actions: tokio::sync::Mutex<()>,
}
impl Overseer {
    pub(crate) fn from_env(port: u16) -> anyhow::Result<Arc<Self>> {
        let specs: Vec<ServiceSpec> = match std::env::var_os("RUSTYKRAB_SERVICE_RESOURCES") {
            Some(path) => serde_json::from_slice(&std::fs::read(path)?)?,
            None => vec![],
        };
        validate(&specs, port)?;
        let observations = specs
            .iter()
            .map(|s| ServiceObservation {
                id: s.id.clone(),
                role: s.role.clone(),
                version: None,
                healthy: None,
                checked_at: None,
                process_id: None,
                supervised: false,
                identity_verified: false,
                lifecycle_safe: false,
                supervisor: s.launchd_label.clone(),
                ensure_running: s.ensure_running,
                detail: "Not probed yet".into(),
            })
            .collect();
        Ok(Arc::new(Self {
            processes: Arc::new(crate::update_cmd::apply::SystemProcesses),
            specs,
            #[cfg(test)]
            launchctl_path: PathBuf::from("/bin/launchctl"),
            observations: RwLock::new(observations),
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(3))
                .build()?,
            actions: tokio::sync::Mutex::new(()),
        }))
    }
    fn spec(&self, id: &str) -> Option<&ServiceSpec> {
        self.specs.iter().find(|s| s.id == id)
    }
    fn target(spec: &ServiceSpec) -> String {
        // SAFETY: geteuid has no arguments and no memory effects.
        format!("gui/{}/{}", unsafe { libc::geteuid() }, spec.launchd_label)
    }
    async fn command(&self, args: &[&str]) -> Result<std::process::Output> {
        tokio::time::timeout(
            Duration::from_secs(10),
            tokio::process::Command::new({
                #[cfg(test)]
                {
                    &self.launchctl_path
                }
                #[cfg(not(test))]
                {
                    std::path::Path::new("/bin/launchctl")
                }
            })
            .args(args)
            .kill_on_drop(true)
            .output(),
        )
        .await
        .map_err(|_| Error::Internal("launchctl timed out".into()))?
        .map_err(|e| Error::Internal(e.to_string()))
    }
    async fn probe(&self, s: &ServiceSpec) -> ServiceObservation {
        // Health is public but the gateway still checks the request origin.
        let origin = reqwest::Url::parse(&s.health_url)
            .expect("validated health URL")
            .origin()
            .ascii_serialization();
        let healthy = self
            .client
            .get(&s.health_url)
            .header(reqwest::header::ORIGIN, origin)
            .send()
            .await
            .is_ok_and(|r| r.status().is_success());
        // launchctl output can contain credentials. Only parse state and pid;
        // never return, log or include raw output in an error.
        let target = Self::target(s);
        let (pid, running) = match self.command(&["print", &target]).await {
            Ok(out) if out.status.success() => {
                let text = String::from_utf8_lossy(&out.stdout);
                let pid = text
                    .lines()
                    .find_map(|l| l.trim().strip_prefix("pid = ").and_then(|p| p.parse().ok()));
                (pid, text.lines().any(|l| l.trim() == "state = running"))
            }
            _ => (None, false),
        };
        let processes = self.processes.clone();
        let binary = s.binary.clone();
        let port = reqwest::Url::parse(&s.health_url)
            .expect("validated health URL")
            .port()
            .expect("validated port");
        let (listener_pid, identity_verified, absent_verified) =
            tokio::task::spawn_blocking(move || {
                let Ok(listeners) = processes.listeners(port) else {
                    return (None, false, false);
                };
                let ids: std::collections::BTreeSet<u32> =
                    listeners.iter().map(|l| l.pid).collect();
                let listener_pid = if ids.len() == 1 {
                    ids.iter().next().copied()
                } else {
                    None
                };
                let candidate = listener_pid.or(pid);
                let loopback = listeners
                    .iter()
                    .all(|l| crate::update_cmd::apply::is_loopback_address(&l.address));
                let same = candidate
                    .and_then(|p| processes.executable(p).ok())
                    .and_then(|p| p.canonicalize().ok())
                    .zip(binary.canonicalize().ok())
                    .is_some_and(|(a, b)| a == b);
                (
                    listener_pid,
                    same && loopback && ids.len() <= 1,
                    listeners.is_empty() && pid.is_none() && !running && !healthy,
                )
            })
            .await
            .unwrap_or((None, false, false));
        let supervised = running
            && pid.is_some()
            && (listener_pid == pid || (!healthy && listener_pid.is_none()))
            && identity_verified;
        let version = match tokio::time::timeout(
            Duration::from_secs(5),
            tokio::process::Command::new(&s.binary)
                .arg("--version")
                .kill_on_drop(true)
                .output(),
        )
        .await
        {
            Ok(Ok(o)) if o.status.success() => Some(
                String::from_utf8_lossy(&o.stdout)
                    .trim()
                    .chars()
                    .take(160)
                    .collect(),
            ),
            _ => None,
        };
        ServiceObservation {
            id: s.id.clone(),
            role: s.role.clone(),
            version,
            healthy: Some(healthy && identity_verified),
            checked_at: Some(Utc::now()),
            process_id: listener_pid.or(pid),
            supervised,
            identity_verified,
            lifecycle_safe: supervised || absent_verified,
            supervisor: s.launchd_label.clone(),
            ensure_running: s.ensure_running,
            detail: if healthy {
                if supervised {
                    "Healthy; launchd running"
                } else {
                    "Health answers; listener identity or supervisor ownership needs attention"
                }
            } else if running {
                "Health request failed; launchd process still running"
            } else {
                "Health request failed; launchd job not running"
            }
            .into(),
        }
    }
    async fn observe(&self) -> Vec<ServiceObservation> {
        let mut rows = Vec::new();
        for s in &self.specs {
            rows.push(self.probe(s).await);
        }
        *self.observations.write().unwrap_or_else(|e| e.into_inner()) = rows.clone();
        rows
    }
    pub(crate) fn start(
        self: &Arc<Self>,
        store: Store,
        control: Arc<dyn ControlHandle>,
    ) -> tokio::task::JoinHandle<()> {
        let own = self.clone();
        tokio::spawn(async move {
            let mut timer = tokio::time::interval(SERVICE_CHECK_EVERY);
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                timer.tick().await;
                let rows = own.observe().await;
                if !control
                    .loop_status()
                    .is_some_and(|s| s.lock == Some(LockState::Held) && !s.draining)
                {
                    continue;
                }
                let Ok(work) = store.work_monitor_snapshot(500, 0).await else {
                    continue;
                };
                for row in rows
                    .into_iter()
                    .filter(|r| recovery_allowed(r, &work, Utc::now()))
                {
                    let a = ServiceAction {
                        resource: row.id.clone(),
                        action: "ensure_running".into(),
                    };
                    match control
                        .file_draft(
                            a.draft(),
                            Provenance {
                                conversation_id: None,
                                filed_by_item: None,
                                actor: "supervisor".into(),
                            },
                        )
                        .await
                    {
                        Ok(PlanOutcome::Accepted(p)) => {
                            tracing::warn!(resource=%row.id,item=%p.root,"service recovery filed")
                        }
                        Ok(_) => tracing::warn!(resource=%row.id,"service recovery refused"),
                        Err(_) => {
                            tracing::warn!(resource=%row.id,"service recovery could not be filed")
                        }
                    }
                }
            }
        })
    }
}
impl ResourceObserver for Overseer {
    fn services(&self) -> Vec<ServiceObservation> {
        self.observations
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    fn registered(&self, id: &str) -> bool {
        self.spec(id).is_some()
    }
}
fn action_of(refs: &[ArtifactRef]) -> Option<ServiceAction> {
    let mut actions = refs.iter().filter(|r| r.kind == SERVICE_ACTION);
    let action = serde_json::from_str::<ServiceAction>(&actions.next()?.value).ok()?;
    (actions.next().is_none() && action.valid()).then_some(action)
}

fn recovery_allowed(
    row: &ServiceObservation,
    work: &rustykrab_store::WorkMonitorSnapshot,
    now: chrono::DateTime<Utc>,
) -> bool {
    if work.items_truncated
        || !row.ensure_running
        || row.healthy != Some(false)
        || row.process_id.is_some()
        || !row.lifecycle_safe
    {
        return false;
    }
    let recent: Vec<_> = work
        .items
        .iter()
        .filter(|w| action_of(&w.item.artifact_refs).is_some_and(|a| a.resource == row.id))
        .collect();
    // Open actions survive controller restart; failed/closed attempts impose
    // backoff and a finite hourly ceiling rather than an invisible restart loop.
    !recent.iter().any(|w| !w.item.status.is_closed())
        && !recent
            .iter()
            .any(|w| now - w.item.created_at < TimeDelta::seconds(120))
        && recent
            .iter()
            .filter(|w| now - w.item.created_at < TimeDelta::hours(1))
            .count()
            < 3
}

pub(crate) struct InfrastructureWorker(Arc<Overseer>, tokio::sync::Mutex<Option<Instant>>);
impl InfrastructureWorker {
    pub(crate) fn new(overseer: Arc<Overseer>) -> Self {
        Self(overseer, tokio::sync::Mutex::new(None))
    }
}
const SERVICE_CHECK_EVERY: Duration = Duration::from_secs(15);
#[async_trait]
impl Worker for InfrastructureWorker {
    fn name(&self) -> &str {
        "infrastructure"
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::Local
    }
    fn accepts(&self, item: &WorkItem) -> bool {
        item.kind == rustykrab_core::work::WorkKind::Personal
            && item.writable_resources.is_empty()
            && item.required_tools == [SERVICE_TOOL]
            && action_of(&item.artifact_refs).is_some_and(|a| self.0.registered(&a.resource))
    }
    fn capabilities(&self) -> WorkerCapabilities {
        WorkerCapabilities {
            tools: vec![SERVICE_TOOL.into()],
            machine: Some("local-services".into()),
            ..Default::default()
        }
    }
    async fn refresh(&self) -> bool {
        let mut checked = self.1.lock().await;
        if checked.is_some_and(|at| at.elapsed() < SERVICE_CHECK_EVERY) {
            return false;
        }
        // The adapter must remain available to recover an absent service.
        // Service health is reported separately; this heartbeat confirms that
        // the registered resources have a current host observation.
        let rows = self.0.services();
        let fresh = !rows.is_empty()
            && rows.iter().all(|row| {
                row.checked_at.is_some_and(|at| {
                    (Utc::now() - at)
                        .to_std()
                        .is_ok_and(|age| age < SERVICE_CHECK_EVERY)
                })
            });
        if !fresh {
            self.0.observe().await;
        }
        *checked = Some(Instant::now());
        true
    }
    async fn run(&self, brief: Brief) -> Result<ResultReport> {
        let _guard = self.0.actions.lock().await;
        let a = action_of(&brief.artifact_refs)
            .ok_or_else(|| Error::Config("invalid service action".into()))?;
        let s = self
            .0
            .spec(&a.resource)
            .ok_or_else(|| Error::Config("unregistered service".into()))?;
        let before = self.0.probe(s).await;
        if !before.lifecycle_safe {
            return Err(Error::Config("Existing listener is not owned by the configured service; no lifecycle command applied".into()));
        }
        let target = Overseer::target(s);
        let mut commands = vec![];
        if a.action == "restart" || before.healthy != Some(true) {
            if !self.0.command(&["print", &target]).await?.status.success() {
                let domain = target.rsplit_once('/').expect("target domain").0;
                let plist = s
                    .plist
                    .to_str()
                    .ok_or_else(|| Error::Config("invalid plist path".into()))?;
                let out = self.0.command(&["bootstrap", domain, plist]).await?;
                if !out.status.success() {
                    return Err(Error::Internal(format!(
                        "service {} bootstrap failed (exit {:?})",
                        s.id,
                        out.status.code()
                    )));
                }
                commands.push(ArtifactRef {
                    kind: COMMAND_RUN.into(),
                    value: format!("launchctl bootstrap {domain} {plist}"),
                });
            }
            let flag = if a.action == "restart" { "-k" } else { "-p" };
            let out = self.0.command(&["kickstart", flag, &target]).await?;
            if !out.status.success() {
                return Err(Error::Internal(format!(
                    "service {} kickstart failed (exit {:?})",
                    s.id,
                    out.status.code()
                )));
            }
            commands.push(ArtifactRef {
                kind: COMMAND_RUN.into(),
                value: format!("launchctl kickstart {flag} {target}"),
            });
        }
        for _ in 0..10 {
            let row = self.0.probe(s).await;
            if row.healthy == Some(true) && row.supervised && row.identity_verified {
                commands.push(ArtifactRef {
                    kind: "service_observation".into(),
                    value: serde_json::to_string(&row)
                        .map_err(|e| Error::Internal(e.to_string()))?,
                });
                return Ok(ResultReport {
                    summary: format!(
                        "{} is healthy under {} (PID {}).",
                        s.id,
                        s.launchd_label,
                        row.process_id.unwrap_or_default()
                    ),
                    artifacts: commands,
                    ..Default::default()
                });
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        Err(Error::Internal(format!(
            "service {} did not become healthy under launchd",
            s.id
        )))
    }
}

pub(crate) struct ResourcesTool {
    pub store: Store,
    pub registry: Arc<WorkerRegistry>,
    pub overseer: Arc<Overseer>,
}
#[async_trait]
impl Tool for ResourcesTool {
    fn name(&self) -> &str {
        "work_resources"
    }
    fn description(&self) -> &str {
        "Inspect available agents, registered services, durable projects and scheduled jobs before allocating work."
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().into(),
            description: self.description().into(),
            parameters: json!({"type":"object","properties":{"project_id":{"type":"string","description":"Optional project UUID: inspect its immutable current planning snapshot."}},"additionalProperties":false}),
        }
    }
    async fn execute(&self, args: Value) -> Result<Value> {
        let project = match args.get("project_id").and_then(Value::as_str) {
            Some(id) => {
                let id = id
                    .parse()
                    .map_err(|_| Error::Config("invalid project UUID".into()))?;
                self.store.projects().get(&id).await?
            }
            None => None,
        };
        Ok(
            json!({"project":project,"workers":self.registry.views().await?,"services":self.overseer.services(),"projects":self.store.projects().list().await?,"schedules":self.store.jobs().list_jobs().await?}),
        )
    }
}
pub(crate) struct ServiceTool {
    pub control: Arc<dyn ControlHandle>,
    pub overseer: Arc<Overseer>,
}
#[async_trait]
impl Tool for ServiceTool {
    fn name(&self) -> &str {
        SERVICE_TOOL
    }
    fn description(&self) -> &str {
        "File a tracked ensure_running or restart action for an operator-registered service. This queues work; it does not claim completion."
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().into(),
            description: self.description().into(),
            parameters: json!({"type":"object","required":["resource","action"],"additionalProperties":false,"properties":{"resource":{"type":"string"},"action":{"enum":["ensure_running","restart"]}}}),
        }
    }
    async fn execute(&self, args: Value) -> Result<Value> {
        let a: ServiceAction = serde_json::from_value(args)
            .map_err(|_| Error::Config("invalid service action".into()))?;
        if !a.valid() || !self.overseer.registered(&a.resource) {
            return Err(Error::Config("unregistered service or action".into()));
        }
        let outcome = self
            .control
            .file_draft(a.draft(), rustykrab_tools::work_backend::host_provenance())
            .await?;
        serde_json::to_value(outcome).map_err(|e| Error::Internal(e.to_string()))
    }
    fn blocks_turn(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustykrab_control::controller::{Controller, ControllerConfig};
    use rustykrab_core::work::{Status, WorkItemDraft};
    use std::os::unix::fs::PermissionsExt;
    struct FixtureProcesses {
        running: PathBuf,
        binary: PathBuf,
    }
    impl crate::update_cmd::apply::Processes for FixtureProcesses {
        fn listeners(&self, port: u16) -> anyhow::Result<Vec<crate::update_cmd::apply::Listener>> {
            Ok(if self.running.is_file() {
                vec![crate::update_cmd::apply::Listener {
                    pid: 12345,
                    address: format!("127.0.0.1:{port}"),
                }]
            } else {
                vec![]
            })
        }
        fn executable(&self, _: u32) -> anyhow::Result<PathBuf> {
            Ok(self.binary.clone())
        }
        fn terminate(&self, _: u32) -> anyhow::Result<()> {
            panic!("observer must not terminate")
        }
        fn alive(&self, _: u32) -> bool {
            self.running.is_file()
        }
    }
    struct Fixture {
        dir: tempfile::TempDir,
        owner: Arc<Overseer>,
        server: tokio::task::JoinHandle<()>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.server.abort();
        }
    }
    async fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("running");
        let launch = dir.path().join("launchctl");
        // Fixture paths are generated by tempfile and contain no shell metacharacters.
        std::fs::write(&launch,format!("#!/bin/sh\ncase \"$1\" in\nprint) if test -f '{}'; then printf 'state = running\\npid = 12345\\nSECRET_VALUE=must-not-appear\\n'; exit 0; else exit 1; fi;;\nbootstrap|kickstart) touch '{}'; exit 0;;\n*) exit 2;;\nesac\n",state.display(),state.display())).unwrap();
        std::fs::set_permissions(&launch, std::fs::Permissions::from_mode(0o700)).unwrap();
        let plist = dir.path().join("fixture.plist");
        std::fs::write(&plist, "fixture").unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let expected_origin = format!("http://127.0.0.1:{port}");
        let app = axum::Router::new().route(
            "/api/health",
            axum::routing::get(move |headers: axum::http::HeaderMap| {
                let state = state.clone();
                let expected_origin = expected_origin.clone();
                async move {
                    if headers
                        .get(axum::http::header::ORIGIN)
                        .and_then(|v| v.to_str().ok())
                        != Some(expected_origin.as_str())
                    {
                        return axum::http::StatusCode::FORBIDDEN;
                    }
                    if state.is_file() {
                        axum::http::StatusCode::OK
                    } else {
                        axum::http::StatusCode::SERVICE_UNAVAILABLE
                    }
                }
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let spec = ServiceSpec {
            id: "fixture".into(),
            role: "test infrastructure".into(),
            health_url: format!("http://127.0.0.1:{port}/api/health"),
            binary: PathBuf::from("/usr/bin/true"),
            launchd_label: "test.fixture".into(),
            plist,
            ensure_running: true,
        };
        validate(std::slice::from_ref(&spec), 3311).unwrap();
        let owner = Arc::new(Overseer {
            processes: Arc::new(FixtureProcesses {
                running: dir.path().join("running"),
                binary: PathBuf::from("/usr/bin/true"),
            }),
            specs: vec![spec],
            launchctl_path: launch,
            observations: RwLock::new(vec![]),
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            actions: tokio::sync::Mutex::new(()),
        });
        Fixture { dir, owner, server }
    }
    #[tokio::test]
    async fn infrastructure_executes_a_tracked_action_and_verifies_health_without_taking_generic_work(
    ) {
        let f = fixture().await;
        let unscoped = reqwest::get(&f.owner.specs[0].health_url).await.unwrap();
        assert_eq!(unscoped.status(), reqwest::StatusCode::FORBIDDEN);
        let store = Store::open(f.dir.path().join("db"), vec![7; 32]).unwrap();
        let worker = Arc::new(InfrastructureWorker::new(f.owner.clone()));
        let controller = Controller::new(store.clone(), vec![worker], ControllerConfig::default());
        let plain = controller
            .file_draft(
                WorkItemDraft {
                    title: "Generic task".into(),
                    objective: "Must go to an agent".into(),
                    done_when: "The agent returns a result".into(),
                    ..Default::default()
                },
                Provenance {
                    conversation_id: None,
                    filed_by_item: None,
                    actor: "user:test".into(),
                },
            )
            .await
            .unwrap();
        let action = ServiceAction {
            resource: "fixture".into(),
            action: "ensure_running".into(),
        };
        let accepted = controller
            .file_draft(
                action.draft(),
                Provenance {
                    conversation_id: None,
                    filed_by_item: None,
                    actor: "user:test".into(),
                },
            )
            .await
            .unwrap();
        let PlanOutcome::Accepted(a) = accepted else {
            panic!("action refused")
        };
        for _ in 0..100 {
            controller.tick().await.unwrap();
            if store.work_get(&a.root).await.unwrap().unwrap().status == Status::Done {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let item = store.work_get(&a.root).await.unwrap().unwrap();
        assert_eq!(item.status, Status::Done);
        let leases = store.work_lease_history(&a.root).await.unwrap();
        assert_eq!(leases[0].lease.worker, "infrastructure");
        let evidence = store.work_evidence_list(&a.root).await.unwrap();
        let rendered = serde_json::to_string(&evidence).unwrap();
        assert!(rendered.contains("service_observation"));
        assert!(rendered.contains("bootstrap"));
        assert!(!rendered.contains("SECRET_VALUE"));
        let PlanOutcome::Accepted(p) = plain else {
            panic!("plain refused")
        };
        assert!(store.work_lease_history(&p.root).await.unwrap().is_empty());
        let observed = f.owner.observe().await;
        assert_eq!(observed[0].healthy, Some(true));
        let before = observed[0].checked_at;
        assert_eq!(
            f.owner.services()[0].checked_at,
            before,
            "reading must not advance probe time"
        );
    }
    #[tokio::test]
    async fn infrastructure_refresh_updates_registry_health_without_running_lifecycle_actions() {
        let f = fixture().await;
        let store = Store::open(f.dir.path().join("db"), vec![7; 32]).unwrap();
        let registry = WorkerRegistry::new(store.clone());
        let worker = Arc::new(InfrastructureWorker::new(f.owner.clone()));
        registry
            .register(worker.clone(), json!({"role":"infrastructure"}), None)
            .await
            .unwrap();
        let first_probe = f.owner.services()[0].checked_at;
        assert!(first_probe.is_some());
        assert_eq!(f.owner.services()[0].healthy, Some(false));
        assert!(
            registry.refresh().await.unwrap().is_empty(),
            "recent checks are throttled"
        );
        store
            .workers()
            .advertise(
                "infrastructure",
                None,
                "healthy",
                Some(Utc::now() - TimeDelta::minutes(3)),
            )
            .await
            .unwrap();
        *worker.1.lock().await = Some(Instant::now() - SERVICE_CHECK_EVERY);
        assert_eq!(registry.refresh().await.unwrap(), ["infrastructure"]);
        let view = registry.view("infrastructure").await.unwrap().unwrap();
        assert!(
            view.healthy,
            "an absent managed service must remain recoverable"
        );
        assert!(Utc::now() - view.last_seen.unwrap() < TimeDelta::seconds(5));
        assert_eq!(
            f.owner.services()[0].checked_at,
            first_probe,
            "fresh cached probes are reused"
        );
        assert!(
            !f.dir.path().join("running").exists(),
            "health refresh must not apply a lifecycle action"
        );
    }
    #[tokio::test]
    async fn automatic_recovery_uses_durable_open_attempts_backoff_and_hourly_ceiling() {
        let f = fixture().await;
        let store = Store::open(f.dir.path().join("db"), vec![7; 32]).unwrap();
        let ctl = Controller::new(store.clone(), vec![], ControllerConfig::default());
        let row = f.owner.probe(&f.owner.specs[0]).await;
        let now = Utc::now();
        let snap = store.work_monitor_snapshot(500, 0).await.unwrap();
        assert!(recovery_allowed(&row, &snap, now));
        for attempt in 0..3 {
            let PlanOutcome::Accepted(a) = ctl
                .file_draft(
                    ServiceAction {
                        resource: "fixture".into(),
                        action: "ensure_running".into(),
                    }
                    .draft(),
                    Provenance {
                        conversation_id: None,
                        filed_by_item: None,
                        actor: "supervisor".into(),
                    },
                )
                .await
                .unwrap()
            else {
                panic!("recovery rejected")
            };
            let snap = store.work_monitor_snapshot(500, 0).await.unwrap();
            assert!(
                !recovery_allowed(&row, &snap, now + TimeDelta::minutes(3)),
                "an open recovery must prevent duplicate dispatch"
            );
            ctl.cancel(&a.root, Some("fixture failed attempt".into()), "user:test")
                .await
                .unwrap();
            let fresh = Store::open(f.dir.path().join("db"), vec![7; 32]).unwrap();
            let mut snap = fresh.work_monitor_snapshot(500, 0).await.unwrap();
            assert!(
                !recovery_allowed(&row, &snap, now + TimeDelta::seconds(60)),
                "restart must retain backoff"
            );
            assert_eq!(
                recovery_allowed(&row, &snap, now + TimeDelta::minutes(3)),
                attempt < 2
            );
            snap.items_truncated = true;
            assert!(!recovery_allowed(&row, &snap, now + TimeDelta::hours(2)));
        }
        let snap = store.work_monitor_snapshot(500, 0).await.unwrap();
        assert!(recovery_allowed(&row, &snap, now + TimeDelta::hours(2)));
    }
    #[tokio::test]
    async fn unrelated_listener_cannot_be_restarted_or_pass_verification() {
        let mut f = fixture().await;
        std::fs::write(f.dir.path().join("running"), "existing-process").unwrap();
        Arc::get_mut(&mut f.owner).unwrap().processes = Arc::new(FixtureProcesses {
            running: f.dir.path().join("running"),
            binary: PathBuf::from("/usr/bin/false"),
        });
        let observation = f.owner.probe(&f.owner.specs[0]).await;
        assert_eq!(observation.healthy, Some(false));
        assert!(!observation.identity_verified && !observation.lifecycle_safe);
        let store = Store::open(f.dir.path().join("db"), vec![7; 32]).unwrap();
        let ctl = Controller::new(
            store.clone(),
            vec![Arc::new(InfrastructureWorker::new(f.owner.clone()))],
            ControllerConfig::default(),
        );
        let PlanOutcome::Accepted(a) = ctl
            .file_draft(
                ServiceAction {
                    resource: "fixture".into(),
                    action: "restart".into(),
                }
                .draft(),
                Provenance {
                    conversation_id: None,
                    filed_by_item: None,
                    actor: "user:test".into(),
                },
            )
            .await
            .unwrap()
        else {
            panic!("action rejected")
        };
        for _ in 0..100 {
            ctl.tick().await.unwrap();
            if !store.work_evidence_list(&a.root).await.unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!store.work_lease_history(&a.root).await.unwrap().is_empty());
        assert_ne!(
            store.work_get(&a.root).await.unwrap().unwrap().status,
            Status::Done
        );
        assert!(!store
            .work_evidence_list(&a.root)
            .await
            .unwrap()
            .iter()
            .any(|e| e.kind == COMMAND_RUN || e.kind == "service_observation"));
        assert_eq!(
            std::fs::read_to_string(f.dir.path().join("running")).unwrap(),
            "existing-process"
        );
    }
    #[tokio::test]
    async fn resource_configuration_refuses_remote_targets_self_port_and_duplicate_ids() {
        let f = fixture().await;
        let mut s = f.owner.specs[0].clone();
        s.health_url = "https://example.com/api/health".into();
        assert!(validate(&[s.clone()], 3311).is_err());
        s.health_url = "http://127.0.0.1:3311/api/health".into();
        assert!(validate(&[s.clone()], 3311).is_err());
        s = f.owner.specs[0].clone();
        assert!(validate(&[s.clone(), s.clone()], 3311).is_err());
        s.launchd_label = "evil; touch /tmp/file".into();
        assert!(validate(&[s], 3311).is_err());
        assert!(action_of(&[ArtifactRef {
            kind: SERVICE_ACTION.into(),
            value: r#"{"resource":"fixture","action":"shell","command":"touch /tmp/file"}"#.into()
        }])
        .is_none());
    }
}
