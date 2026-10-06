//! The worker registry (plan sections 5 and 13): the named workers the
//! controller may lease to, persisted in the store's `workers` table.
//!
//! Names are the registry's. A worker added without one, and the local
//! worker when it first registers itself, gets the first free name of
//! [`NAME_POOL`]; a name the user asks for is kept if it is free and
//! well-formed. The name is what leases, events and channel messages carry
//! (`worker:<name>`), so it never changes once given: the local worker
//! reclaims its row's name on every start.
//!
//! Two kinds of worker live here. The daemon builds its own (the local
//! worker) and [`WorkerRegistry::register`]s it. External workers
//! (`claude_code`, `codex`) and peers (a paired node, Phase 5) are
//! described by a [`WorkerSpec`], built by the injected [`WorkerFactory`],
//! and their spec is stored as the row's `config`, so
//! [`WorkerRegistry::restore`] rebuilds them after a restart. A peer's spec
//! names its node's `base_url`; its token (given, or redeemed from a
//! pairing code by [`WorkerFactory::prepare`]) is kept in the store's
//! encrypted secrets under [`crate::peer::token_secret`], never in the
//! stored spec or a [`WorkerView`].
//!
//! The registry also caches each worker's cost tier and routing record so
//! the controller's match step reads them without a store round trip;
//! [`WorkerRegistry::update_record`] writes the record through the store's
//! one-transaction update and refreshes the cache.
//! [`WorkerRegistry::refresh`] asks each worker where it stands and records
//! a peer's advertisement (models, tools, MCP servers, machine) and health
//! on its row.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Utc};
use rustykrab_core::work::WorkerKind;
use rustykrab_core::Error;
use rustykrab_store::{ClassRecord, RoutingRecord, Store, WorkerRow, WorkerUpsert, WriteAuthority};
use serde::{Deserialize, Serialize};

use crate::controller::default_cost_tier;
use crate::peer::token_secret;
use crate::worker::{Worker, WorkerCapabilities};

/// Names the registry gives out, in order. The local worker, registering
/// first on a fresh daemon, is `snapper`. The plan's own examples, `pinch`
/// and `krabby`, are left for the user to give ("give that one to
/// pinch").
pub const NAME_POOL: [&str; 10] = [
    "snapper", "hermit", "coral", "shelly", "barnacle", "kelp", "limpet", "nipper", "scuttle",
    "reef",
];

/// The longest name the registry accepts.
const NAME_MAX: usize = 32;

/// What `rustykrab worker add` and `POST /api/workers` describe: an
/// external worker and the limits its adapter runs under. Stored as the
/// worker's `config`, so the same spec rebuilds it after a restart.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerSpec {
    pub kind: WorkerKind,
    /// The name asked for; the registry assigns one when it is `None`.
    #[serde(default)]
    pub name: Option<String>,
    /// Repositories the worker may work in. Each becomes a `repo:<path>`
    /// writable resource it covers.
    #[serde(default)]
    pub repos: Vec<String>,
    /// The executable (`claude`, `codex`, or a path).
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// Native Claude login profile; None selects the CLI default login.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_config_dir: Option<String>,
    /// Require a first-party claude.ai Max login, excluding API billing.
    #[serde(default)]
    pub require_max: bool,
    /// Native Codex login profile. None uses the CLI default ~/.codex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_home: Option<String>,
    /// Require Sign in with ChatGPT and exclude API/provider fallback.
    #[serde(default)]
    pub require_chatgpt: bool,
    /// The agent's own tool allowlist; empty takes the adapter's default.
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// Rules denied on top of the adapter's own deny list, such as
    /// `Read(~/.config/**)`. Claude Code only; codex has no equivalent and
    /// ignores them with a warning.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub denied_tools: Vec<String>,
    #[serde(default)]
    pub max_turns: Option<u32>,
    #[serde(default)]
    pub permission_mode: Option<String>,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    #[serde(default)]
    pub concurrency: Option<usize>,
    /// Overrides the kind's default cost tier.
    #[serde(default)]
    pub cost_tier: Option<u32>,
    /// Environment variables passed through to the agent's process, by
    /// name. Nothing else of the daemon's environment is.
    #[serde(default)]
    pub env: Vec<String>,
    /// A peer's node: its gateway's base URL, reached over the tailnet or a
    /// tunnel (`https://node.tailnet.ts.net`, `http://127.0.0.1:3100`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// A peer's bearer token for its node: a device token from pairing, or
    /// the node's own. Taken on `add` and kept in the secret store; never
    /// serialised, so it is in neither the stored spec nor a view.
    #[serde(default, skip_serializing)]
    pub token: Option<String>,
    /// A one-time code the node printed (`rustykrab-cli pair`), redeemed on
    /// `add` for a device token of this daemon's own. Never serialised.
    #[serde(default, skip_serializing)]
    pub pairing_code: Option<String>,
}

/// Builds the worker a spec describes, under the name the registry gave
/// it. The composition root implements it over the adapters in
/// `rustykrab-agent`.
#[async_trait::async_trait]
pub trait WorkerFactory: Send + Sync {
    fn build(&self, name: &str, spec: &WorkerSpec) -> Result<Arc<dyn Worker>, String>;

    /// Complete a spec on `add`, before its worker is built and stored,
    /// with what needs the network: a peer's pairing code redeemed at its
    /// node for a device token. Default: the spec as given.
    async fn prepare(&self, _name: &str, spec: WorkerSpec) -> Result<WorkerSpec, String> {
        Ok(spec)
    }
}

/// One worker as `GET /api/workers` and `rustykrab workers` show it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerView {
    pub name: String,
    pub kind: String,
    /// Built and leasable in this process.
    pub live: bool,
    pub healthy: bool,
    pub health: String,
    pub last_seen: Option<DateTime<Utc>>,
    pub cost_tier: u32,
    pub concurrency: usize,
    pub capabilities: WorkerCapabilities,
    /// Keyed by work class; the shape is `rustykrab_store::ClassRecord`.
    pub routing_record: RoutingRecord,
    /// The spec an external worker was added with.
    #[serde(default)]
    pub spec: Option<WorkerSpec>,
    /// Live runtime observations (subscription, hashed account, cooldown).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
}

/// The named workers of this daemon.
pub struct WorkerRegistry {
    store: Store,
    factory: RwLock<Option<Arc<dyn WorkerFactory>>>,
    /// Leasable workers, in registration order (the match step's tie
    /// break).
    live: RwLock<Vec<Arc<dyn Worker>>>,
    tiers: RwLock<HashMap<String, u32>>,
    records: RwLock<HashMap<String, RoutingRecord>>,
    /// Serialises name assignment, so two adds cannot take one name.
    naming: tokio::sync::Mutex<()>,
}

fn lock_err<T>(e: std::sync::PoisonError<T>) -> T {
    e.into_inner()
}

/// Whether `name` is a well-formed worker name: a letter, then letters,
/// digits, `-` or `_`, at most 32 characters.
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= NAME_MAX
        && chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

impl WorkerRegistry {
    pub fn new(store: Store) -> WorkerRegistry {
        WorkerRegistry {
            store,
            factory: RwLock::new(None),
            live: RwLock::new(Vec::new()),
            tiers: RwLock::new(HashMap::new()),
            records: RwLock::new(HashMap::new()),
            naming: tokio::sync::Mutex::new(()),
        }
    }

    /// A registry over fixed workers, in memory only: nothing is written
    /// to the store until a routing record is. What `Controller::new`
    /// builds for tests and scripted daemons.
    pub fn fixed(store: Store, workers: Vec<Arc<dyn Worker>>) -> WorkerRegistry {
        let registry = WorkerRegistry::new(store);
        *registry.live.write().unwrap_or_else(lock_err) = workers;
        registry
    }

    pub fn with_factory(self, factory: Arc<dyn WorkerFactory>) -> WorkerRegistry {
        *self.factory.write().unwrap_or_else(lock_err) = Some(factory);
        self
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// The leasable workers, in registration order.
    pub fn workers(&self) -> Vec<Arc<dyn Worker>> {
        self.live.read().unwrap_or_else(lock_err).clone()
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Worker>> {
        self.live
            .read()
            .unwrap_or_else(lock_err)
            .iter()
            .find(|w| w.name() == name)
            .cloned()
    }

    /// A worker's cost tier: its row's, else its kind's default.
    pub fn cost_tier(&self, worker: &dyn Worker) -> u32 {
        self.tiers
            .read()
            .unwrap_or_else(lock_err)
            .get(worker.name())
            .copied()
            .unwrap_or_else(|| default_cost_tier(worker.kind()))
    }

    /// A worker's record for one class of work, from the cache.
    pub fn record_of(&self, worker: &str, class: &str) -> Option<ClassRecord> {
        self.records
            .read()
            .unwrap_or_else(lock_err)
            .get(worker)
            .and_then(|r| r.get(class))
            .cloned()
    }

    /// Load cost tiers and routing records from the store into the cache.
    pub async fn load(&self) -> Result<(), Error> {
        let rows = self.store.workers().list().await?;
        self.cache(&rows);
        Ok(())
    }

    fn cache(&self, rows: &[WorkerRow]) {
        let mut tiers = self.tiers.write().unwrap_or_else(lock_err);
        let mut records = self.records.write().unwrap_or_else(lock_err);
        for row in rows {
            tiers.insert(row.name.clone(), row.cost_tier);
            records.insert(row.name.clone(), row.routing_record.clone());
        }
    }

    /// The name a new worker gets: `wanted` when it is well-formed and
    /// free, else the first free name of the pool, else `worker-<n>`.
    pub async fn assign_name(&self, wanted: Option<&str>) -> Result<String, Error> {
        let taken: Vec<String> = self
            .store
            .workers()
            .list()
            .await?
            .into_iter()
            .map(|r| r.name)
            .chain(self.workers().iter().map(|w| w.name().to_string()))
            .collect();
        if let Some(name) = wanted {
            let name = name.trim();
            if !valid_name(name) {
                return Err(Error::Config(format!(
                    "invalid worker name `{name}`: a letter, then letters, digits, - or _, \
                     at most {NAME_MAX} characters"
                )));
            }
            if taken.iter().any(|t| t == name) {
                return Err(Error::AlreadyExists(format!("worker {name}")));
            }
            return Ok(name.to_string());
        }
        if let Some(free) = NAME_POOL.iter().find(|n| !taken.iter().any(|t| t == *n)) {
            return Ok((*free).to_string());
        }
        let mut n = taken.len() + 1;
        loop {
            let name = format!("worker-{n}");
            if !taken.contains(&name) {
                return Ok(name);
            }
            n += 1;
        }
    }

    /// The local worker's name: the one its row already has, so it is
    /// stable across restarts, else a newly assigned one.
    pub async fn local_name(&self) -> Result<String, Error> {
        let _naming = self.naming.lock().await;
        let rows = self.store.workers().list().await?;
        if let Some(row) = rows
            .iter()
            .find(|r| r.kind() == Some(WorkerKind::Local) && r.config["role"] != "planner")
        {
            return Ok(row.name.clone());
        }
        self.assign_name(None).await
    }

    /// Make a worker the daemon built itself leasable and record it.
    /// `config` is stored as its row's config; `cost_tier` overrides its
    /// kind's default. The worker is asked where it stands first
    /// ([`Worker::refresh`]), so its row records its health as of now: a
    /// peer that is up is leasable as soon as it is added, and a local
    /// worker whose model is missing is unhealthy from the start.
    pub async fn register(
        &self,
        worker: Arc<dyn Worker>,
        config: serde_json::Value,
        cost_tier: Option<u32>,
    ) -> Result<WorkerView, Error> {
        worker.refresh().await;
        let healthy = worker.healthy();
        let row = self
            .store
            .workers()
            .upsert(WorkerUpsert {
                name: worker.name().to_string(),
                kind: worker.kind(),
                capabilities: serde_json::to_value(worker.capabilities())
                    .unwrap_or(serde_json::Value::Null),
                config,
                health: health_line(worker.as_ref()),
                last_seen: healthy.then(Utc::now),
                cost_tier: cost_tier.unwrap_or_else(|| default_cost_tier(worker.kind())),
            })
            .await?;
        self.cache(std::slice::from_ref(&row));
        {
            let mut live = self.live.write().unwrap_or_else(lock_err);
            live.retain(|w| w.name() != worker.name());
            live.push(worker.clone());
        }
        Ok(view(&row, Some(&worker)))
    }

    /// `worker add`: build an external worker or a peer from `spec` and
    /// register it. A peer needs its node's `base_url` and a `token` or a
    /// `pairing_code`; its node need not be up yet, since the worker is
    /// unhealthy, and takes no lease, until a refresh reads its
    /// advertisement.
    pub async fn add(&self, spec: WorkerSpec) -> Result<WorkerView, Error> {
        if !matches!(
            spec.kind,
            WorkerKind::ClaudeCode | WorkerKind::Codex | WorkerKind::Peer
        ) {
            return Err(Error::Config(format!(
                "`{}` workers are not added by hand: the local worker registers itself; \
                 add claude_code, codex or peer",
                spec.kind.as_str()
            )));
        }
        if spec.kind == WorkerKind::Codex && !spec.denied_tools.is_empty() {
            return Err(Error::Config(format!(
                "codex has no deny list, so denied_tools ({}) would not be enforced; \
                 drop them or add a claude_code worker",
                spec.denied_tools.join(", ")
            )));
        }
        let factory = self
            .factory
            .read()
            .unwrap_or_else(lock_err)
            .clone()
            .ok_or_else(|| Error::Config("this daemon cannot build external workers".into()))?;
        let _naming = self.naming.lock().await;
        let name = self.assign_name(spec.name.as_deref()).await?;
        let spec = if spec.kind == WorkerKind::Peer {
            peer_spec(factory.as_ref(), &name, spec).await?
        } else {
            spec
        };
        let worker = factory.build(&name, &spec).map_err(Error::Config)?;
        if let Some(token) = spec
            .token
            .as_deref()
            .filter(|_| spec.kind == WorkerKind::Peer)
        {
            self.store
                .secrets()
                .upsert_system(&token_secret(&name), token)
                .await?;
        }
        let mut stored = spec.clone();
        stored.name = Some(name);
        let config = serde_json::to_value(&stored).map_err(|e| Error::Internal(e.to_string()))?;
        self.register(worker, config, spec.cost_tier).await
    }

    /// Remove an external worker or a peer: it takes no new leases, and its
    /// row goes, with a peer's token. Its history (leases, events) keeps its
    /// name.
    pub async fn remove(&self, name: &str) -> Result<bool, Error> {
        let row = self.store.workers().get(name).await?;
        if row.as_ref().and_then(WorkerRow::kind) == Some(WorkerKind::Local) {
            return Err(Error::Config(format!(
                "{name} is this daemon's local worker, which registers itself"
            )));
        }
        if row.as_ref().and_then(WorkerRow::kind) == Some(WorkerKind::Peer) {
            self.store
                .secrets()
                .delete(&token_secret(name), WriteAuthority::System)
                .await?;
        }
        let was_live = {
            let mut live = self.live.write().unwrap_or_else(lock_err);
            let before = live.len();
            live.retain(|w| w.name() != name);
            live.len() != before
        };
        let removed = self.store.workers().remove(name).await?;
        self.tiers.write().unwrap_or_else(lock_err).remove(name);
        self.records.write().unwrap_or_else(lock_err).remove(name);
        Ok(removed || was_live)
    }

    /// Rebuild every stored external worker and peer through the factory
    /// (at start), a peer with the token kept for it. A spec the factory
    /// refuses is recorded as the row's health and left out. Returns the
    /// names rebuilt.
    pub async fn restore(&self) -> Result<Vec<String>, Error> {
        let rows = self.store.workers().list().await?;
        self.cache(&rows);
        let factory = self.factory.read().unwrap_or_else(lock_err).clone();
        let mut restored = Vec::new();
        for row in rows {
            if !matches!(
                row.kind(),
                Some(WorkerKind::ClaudeCode | WorkerKind::Codex | WorkerKind::Peer)
            ) {
                continue;
            }
            if self.get(&row.name).is_some() {
                continue;
            }
            let mut spec: Result<WorkerSpec, String> =
                serde_json::from_value(row.config.clone()).map_err(|e| e.to_string());
            if let Ok(spec) = spec.as_mut() {
                if spec.kind == WorkerKind::Peer {
                    spec.token = match self.store.secrets().get(&token_secret(&row.name)).await {
                        Ok(token) => Some(token),
                        Err(Error::NotFound(_)) => None,
                        Err(e) => return Err(e),
                    };
                }
            }
            let built = match (&factory, spec) {
                (Some(f), Ok(spec)) => f.build(&row.name, &spec),
                (None, _) => Err("this daemon cannot build external workers".into()),
                (_, Err(why)) => Err(format!("unreadable spec: {why}")),
            };
            match built {
                Ok(worker) => {
                    let healthy = worker.healthy();
                    self.store
                        .workers()
                        .touch(
                            &row.name,
                            &health_line(worker.as_ref()),
                            healthy.then(Utc::now),
                        )
                        .await?;
                    self.live.write().unwrap_or_else(lock_err).push(worker);
                    restored.push(row.name);
                }
                Err(why) => {
                    tracing::warn!(worker = %row.name, %why, "worker not restored");
                    self.store
                        .workers()
                        .touch(&row.name, &format!("unavailable: {why}"), None)
                        .await?;
                }
            }
        }
        Ok(restored)
    }

    /// Read every worker without touching its last_seen timestamp.
    /// Only registration and successful refreshes attest that a worker answered.
    pub async fn views(&self) -> Result<Vec<WorkerView>, Error> {
        let workers = self.store.workers();
        let mut out = Vec::new();
        for row in workers.list().await? {
            let live = self.get(&row.name);
            out.push(view(&row, live.as_ref()));
        }
        // Workers built in memory only (a fixed registry) have no row.
        for w in self.workers() {
            if !out.iter().any(|v| v.name == w.name()) {
                out.push(WorkerView {
                    name: w.name().to_string(),
                    kind: w.kind().as_str().to_string(),
                    live: true,
                    healthy: w.healthy(),
                    health: health_line(w.as_ref()),
                    last_seen: None,
                    cost_tier: self.cost_tier(w.as_ref()),
                    concurrency: w.concurrency(),
                    capabilities: w.capabilities(),
                    routing_record: self
                        .records
                        .read()
                        .unwrap_or_else(lock_err)
                        .get(w.name())
                        .cloned()
                        .unwrap_or_default(),
                    spec: None,
                    runtime: w.runtime_status(),
                    created_at: Utc::now(),
                });
            }
        }
        Ok(out)
    }

    pub async fn view(&self, name: &str) -> Result<Option<WorkerView>, Error> {
        Ok(self.views().await?.into_iter().find(|v| v.name == name))
    }

    /// Ask every live worker where it stands ([`Worker::refresh`]) and, for
    /// each that asked, record on its row what it advertises now and how
    /// healthy it is: a peer's models, tools, MCP servers and machine as its
    /// node reported them (plan section 5), kept as last seen while the node
    /// does not answer. The daemon calls this on a timer. Returns the names
    /// recorded.
    pub async fn refresh(&self) -> Result<Vec<String>, Error> {
        let mut recorded = Vec::new();
        for worker in self.workers() {
            if !worker.refresh().await {
                continue;
            }
            let healthy = worker.healthy();
            let capabilities = healthy
                .then(|| serde_json::to_value(worker.capabilities()).ok())
                .flatten();
            self.store
                .workers()
                .advertise(
                    worker.name(),
                    capabilities,
                    &health_line(worker.as_ref()),
                    healthy.then(Utc::now),
                )
                .await?;
            recorded.push(worker.name().to_string());
        }
        Ok(recorded)
    }

    /// Change a worker's routing record in one store transaction and
    /// refresh the cache. A worker with no row (a fixed registry's) gets
    /// one first, so its record is kept.
    pub async fn update_record<F>(&self, name: &str, change: F) -> Result<RoutingRecord, Error>
    where
        F: FnOnce(&mut RoutingRecord) + Send + 'static,
    {
        let workers = self.store.workers();
        if workers.get(name).await?.is_none() {
            let Some(w) = self.get(name) else {
                return Err(Error::NotFound(format!("worker {name}")));
            };
            workers
                .upsert(WorkerUpsert {
                    name: name.to_string(),
                    kind: w.kind(),
                    capabilities: serde_json::to_value(w.capabilities())
                        .unwrap_or(serde_json::Value::Null),
                    config: serde_json::json!({}),
                    health: health_line(w.as_ref()),
                    last_seen: None,
                    cost_tier: self.cost_tier(w.as_ref()),
                })
                .await?;
        }
        let record = workers.update_record(name, change).await?;
        self.records
            .write()
            .unwrap_or_else(lock_err)
            .insert(name.to_string(), record.clone());
        Ok(record)
    }
}

/// A peer's spec made whole for `add`: a well-formed `base_url`, and a
/// token, given or redeemed from a pairing code through the factory. The
/// pairing code goes once used.
async fn peer_spec(
    factory: &dyn WorkerFactory,
    name: &str,
    mut spec: WorkerSpec,
) -> Result<WorkerSpec, Error> {
    let base = spec
        .base_url
        .as_deref()
        .map(|u| u.trim().trim_end_matches('/').to_string())
        .filter(|u| !u.is_empty())
        .ok_or_else(|| Error::Config("a peer needs its node's base_url".into()))?;
    if !(base.starts_with("http://") || base.starts_with("https://")) {
        return Err(Error::Config(format!(
            "a peer's base_url is an http or https URL, not `{base}`"
        )));
    }
    spec.base_url = Some(base);
    spec.token = spec.token.filter(|t| !t.trim().is_empty());
    if spec.token.is_none() && spec.pairing_code.is_some() {
        spec = factory.prepare(name, spec).await.map_err(Error::Config)?;
    }
    spec.pairing_code = None;
    if spec.token.as_deref().is_none_or(|t| t.trim().is_empty()) {
        return Err(Error::Config(
            "a peer needs a token for its node, or a pairing_code to redeem for one".into(),
        ));
    }
    Ok(spec)
}

/// The health recorded on a worker's row: `healthy`, or `unhealthy` with
/// the worker's reason when it gives one (`unhealthy: Ollama at ... has no
/// model `x``).
fn health_line(worker: &dyn Worker) -> String {
    if worker.healthy() {
        return "healthy".to_string();
    }
    match worker
        .unhealthy_reason()
        .map(|why| why.trim().to_string())
        .filter(|why| !why.is_empty())
    {
        Some(why) => format!("unhealthy: {why}"),
        None => "unhealthy".to_string(),
    }
}

fn view(row: &WorkerRow, live: Option<&Arc<dyn Worker>>) -> WorkerView {
    let capabilities = match live {
        Some(w) => w.capabilities(),
        None => serde_json::from_value(row.capabilities.clone()).unwrap_or_default(),
    };
    WorkerView {
        name: row.name.clone(),
        kind: row.kind.clone(),
        live: live.is_some(),
        healthy: live.is_some_and(|w| w.healthy()),
        health: live.map_or_else(|| row.health.clone(), |w| health_line(w.as_ref())),
        last_seen: row.last_seen,
        cost_tier: row.cost_tier,
        concurrency: live.map_or(1, |w| w.concurrency()),
        capabilities,
        routing_record: row.routing_record.clone(),
        spec: serde_json::from_value(row.config.clone())
            .ok()
            .filter(|s: &WorkerSpec| s.kind != WorkerKind::Any),
        runtime: live.and_then(|w| w.runtime_status()),
        created_at: row.created_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use rustykrab_core::work::ResultReport;

    use crate::worker::Brief;

    struct Named {
        name: String,
        kind: WorkerKind,
        repos: Vec<String>,
        /// What a peer was built with, shown as its machine so a test can
        /// see it.
        token: Option<String>,
        refreshes: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl Worker for Named {
        fn name(&self) -> &str {
            &self.name
        }
        fn kind(&self) -> WorkerKind {
            self.kind
        }
        fn capabilities(&self) -> WorkerCapabilities {
            WorkerCapabilities {
                repos: self.repos.clone(),
                machine: self.token.clone(),
                tools: vec!["caldav".to_string()],
                ..WorkerCapabilities::default()
            }
        }
        fn healthy(&self) -> bool {
            self.kind != WorkerKind::Peer
                || self.refreshes.load(std::sync::atomic::Ordering::SeqCst) > 0
        }
        async fn run(&self, _brief: Brief) -> Result<ResultReport, Error> {
            Ok(ResultReport::default())
        }
        async fn refresh(&self) -> bool {
            if self.kind != WorkerKind::Peer {
                return false;
            }
            self.refreshes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            true
        }
    }

    struct Factory;

    #[async_trait]
    impl WorkerFactory for Factory {
        fn build(&self, name: &str, spec: &WorkerSpec) -> Result<Arc<dyn Worker>, String> {
            if spec.command.as_deref() == Some("missing") {
                return Err("command missing".into());
            }
            if spec.kind == WorkerKind::Peer && spec.token.is_none() {
                return Err("no token stored for this peer".into());
            }
            Ok(Arc::new(Named {
                name: name.to_string(),
                kind: spec.kind,
                repos: spec.repos.clone(),
                token: spec.token.clone(),
                refreshes: std::sync::atomic::AtomicUsize::new(0),
            }))
        }

        async fn prepare(&self, name: &str, mut spec: WorkerSpec) -> Result<WorkerSpec, String> {
            match spec.pairing_code.as_deref() {
                Some("PAIR-OK") => {
                    spec.token = Some(format!("device-token-for-{name}"));
                    Ok(spec)
                }
                _ => Err("pairing refused".into()),
            }
        }
    }

    fn temp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("db"), vec![7u8; 32]).unwrap();
        (dir, store)
    }

    fn spec(kind: WorkerKind, name: Option<&str>) -> WorkerSpec {
        WorkerSpec {
            kind,
            name: name.map(str::to_string),
            repos: vec!["/src/app".to_string()],
            command: Some("claude".to_string()),
            ..WorkerSpec::default()
        }
    }

    #[tokio::test]
    async fn names_are_assigned_kept_and_refused_when_taken() {
        let (_dir, store) = temp_store();
        let registry = WorkerRegistry::new(store.clone()).with_factory(Arc::new(Factory));
        let local = registry.local_name().await.unwrap();
        assert_eq!(local, "snapper");
        registry
            .register(
                Arc::new(Named {
                    name: local.clone(),
                    kind: WorkerKind::Local,
                    repos: Vec::new(),
                    token: None,
                    refreshes: std::sync::atomic::AtomicUsize::new(0),
                }),
                serde_json::json!({}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(registry.local_name().await.unwrap(), "snapper", "stable");

        let pinch = registry
            .add(spec(WorkerKind::ClaudeCode, Some("pinch")))
            .await
            .unwrap();
        assert_eq!(pinch.name, "pinch");
        assert_eq!(pinch.cost_tier, 3);
        let taken = registry
            .add(spec(WorkerKind::ClaudeCode, Some("pinch")))
            .await;
        assert!(matches!(taken, Err(Error::AlreadyExists(_))), "{taken:?}");
        let bad = registry
            .add(spec(WorkerKind::Codex, Some("not a name")))
            .await;
        assert!(matches!(bad, Err(Error::Config(_))));
        let assigned = registry.add(spec(WorkerKind::Codex, None)).await.unwrap();
        assert_eq!(assigned.name, "hermit", "the first free pool name");
        assert_eq!(assigned.cost_tier, 2);
        let local_add = registry.add(spec(WorkerKind::Local, None)).await;
        assert!(local_add.is_err(), "the local worker is not added by hand");
        assert!(registry.remove("snapper").await.is_err());

        let names: Vec<String> = registry
            .views()
            .await
            .unwrap()
            .into_iter()
            .map(|v| v.name)
            .collect();
        assert_eq!(names, ["snapper", "pinch", "hermit"]);
    }

    #[tokio::test]
    async fn external_workers_are_rebuilt_from_their_spec_after_a_restart() {
        let (_dir, store) = temp_store();
        let before = WorkerRegistry::new(store.clone()).with_factory(Arc::new(Factory));
        before
            .add(spec(WorkerKind::ClaudeCode, Some("pinch")))
            .await
            .unwrap();
        let mut broken = spec(WorkerKind::Codex, Some("nipper"));
        broken.command = Some("claude".to_string());
        before.add(broken).await.unwrap();
        before
            .update_record("pinch", |r| {
                r.entry("code".to_string()).or_default().verified_done = 4;
            })
            .await
            .unwrap();
        // The codex spec's command goes missing across the restart.
        let conn_store = store.workers();
        let mut row = conn_store.get("nipper").await.unwrap().unwrap();
        row.config["command"] = serde_json::json!("missing");
        conn_store
            .upsert(WorkerUpsert {
                name: row.name.clone(),
                kind: WorkerKind::Codex,
                capabilities: row.capabilities.clone(),
                config: row.config.clone(),
                health: row.health.clone(),
                last_seen: row.last_seen,
                cost_tier: row.cost_tier,
            })
            .await
            .unwrap();

        let after = WorkerRegistry::new(store.clone()).with_factory(Arc::new(Factory));
        assert_eq!(after.restore().await.unwrap(), ["pinch"]);
        let pinch = after.get("pinch").expect("rebuilt");
        assert_eq!(pinch.capabilities().repos, ["/src/app"]);
        assert_eq!(after.record_of("pinch", "code").unwrap().verified_done, 4);
        let nipper = store.workers().get("nipper").await.unwrap().unwrap();
        assert!(
            nipper.health.starts_with("unavailable"),
            "{}",
            nipper.health
        );
        assert!(after.get("nipper").is_none());
        assert!(after.remove("pinch").await.unwrap());
        assert!(after.get("pinch").is_none());
    }

    #[tokio::test]
    async fn denied_tools_round_trip_through_the_stored_spec() {
        let (_dir, store) = temp_store();
        let before = WorkerRegistry::new(store.clone()).with_factory(Arc::new(Factory));
        let mut guarded = spec(WorkerKind::ClaudeCode, Some("pinch"));
        guarded.denied_tools = vec![
            "Read(~/.config/**)".to_string(),
            "Edit(//Users/someone/secrets/**)".to_string(),
        ];
        let view = before.add(guarded.clone()).await.unwrap();
        assert_eq!(view.spec.unwrap().denied_tools, guarded.denied_tools);
        before
            .add(spec(WorkerKind::ClaudeCode, Some("coral")))
            .await
            .unwrap();
        // A spec without the field stores none, as specs did before it.
        let plain = store.workers().get("coral").await.unwrap().unwrap();
        assert!(
            plain.config.get("denied_tools").is_none(),
            "{}",
            plain.config
        );

        let after = WorkerRegistry::new(store.clone()).with_factory(Arc::new(Factory));
        assert_eq!(after.restore().await.unwrap(), ["pinch", "coral"]);
        let views = after.views().await.unwrap();
        let denied = |name: &str| {
            views
                .iter()
                .find(|v| v.name == name)
                .and_then(|v| v.spec.clone())
                .unwrap()
                .denied_tools
        };
        assert_eq!(denied("pinch"), guarded.denied_tools);
        assert!(denied("coral").is_empty());
    }

    #[tokio::test]
    async fn a_codex_spec_with_denied_tools_is_refused_not_silently_dropped() {
        let (_dir, store) = temp_store();
        let registry = WorkerRegistry::new(store.clone()).with_factory(Arc::new(Factory));
        let mut codex = spec(WorkerKind::Codex, Some("squid"));
        codex.denied_tools = vec!["Read(~/.config/**)".to_string()];
        let refused = registry.add(codex).await;
        assert!(
            matches!(&refused, Err(Error::Config(m)) if m.contains("denied_tools")),
            "{refused:?}"
        );
        assert!(store.workers().get("squid").await.unwrap().is_none());
        assert!(registry.get("squid").is_none());

        // Without a deny list the same codex worker is added.
        registry
            .add(spec(WorkerKind::Codex, Some("squid")))
            .await
            .unwrap();
    }

    #[test]
    fn a_stored_spec_from_before_denied_tools_still_reads() {
        let old = serde_json::json!({
            "kind": "claude_code",
            "name": "pinch",
            "repos": ["/src/app"],
            "allowed_tools": ["Read"],
        });
        let spec: WorkerSpec = serde_json::from_value(old).unwrap();
        assert!(spec.denied_tools.is_empty());
        assert_eq!(spec.allowed_tools, ["Read"]);
    }

    fn peer(name: &str) -> WorkerSpec {
        WorkerSpec {
            kind: WorkerKind::Peer,
            name: Some(name.to_string()),
            base_url: Some("http://127.0.0.1:3100/".to_string()),
            ..WorkerSpec::default()
        }
    }

    #[tokio::test]
    async fn a_peer_keeps_its_token_in_the_secret_store_and_comes_back_with_it() {
        let (_dir, store) = temp_store();
        let registry = WorkerRegistry::new(store.clone()).with_factory(Arc::new(Factory));

        // A peer needs a node to reach and a way in.
        let refused = registry.add(peer("krabby")).await;
        assert!(matches!(refused, Err(Error::Config(_))), "{refused:?}");
        let mut ftp = peer("krabby");
        ftp.base_url = Some("ftp://node".to_string());
        ftp.token = Some("t".to_string());
        assert!(matches!(registry.add(ftp).await, Err(Error::Config(_))));

        let mut spec = peer("krabby");
        spec.token = Some("node-token".to_string());
        let view = registry.add(spec).await.unwrap();
        assert_eq!(view.kind, "peer");
        assert_eq!(
            view.cost_tier, 1,
            "peers sit between local work and the agents"
        );
        assert!(
            view.healthy,
            "an added peer is asked for its advertisement at once"
        );
        assert_eq!(
            view.spec.as_ref().and_then(|s| s.base_url.as_deref()),
            Some("http://127.0.0.1:3100")
        );
        assert!(!serde_json::to_string(&view.spec)
            .unwrap()
            .contains("node-token"));
        let row = store.workers().get("krabby").await.unwrap().unwrap();
        assert!(!row.config.to_string().contains("node-token"));
        assert_eq!(row.config["base_url"], "http://127.0.0.1:3100");
        assert_eq!(
            store.secrets().get(&token_secret("krabby")).await.unwrap(),
            "node-token"
        );

        // Pairing redeems the code for a token of this daemon's own.
        let mut paired = peer("nipper");
        paired.pairing_code = Some("PAIR-OK".to_string());
        registry.add(paired).await.unwrap();
        assert_eq!(
            store.secrets().get(&token_secret("nipper")).await.unwrap(),
            "device-token-for-nipper"
        );
        let mut bad = peer("kelp");
        bad.pairing_code = Some("WRONG".to_string());
        assert!(matches!(registry.add(bad).await, Err(Error::Config(_))));

        // After a restart the peer is rebuilt with the token kept for it.
        let after = WorkerRegistry::new(store.clone()).with_factory(Arc::new(Factory));
        assert_eq!(after.restore().await.unwrap(), ["krabby", "nipper"]);
        let krabby = after.get("krabby").expect("rebuilt");
        assert_eq!(krabby.capabilities().machine.as_deref(), Some("node-token"));
        assert!(!krabby.healthy(), "not healthy until it answers again");

        // A refresh records what it advertises on its row.
        store
            .workers()
            .advertise("krabby", Some(serde_json::json!({})), "unhealthy", None)
            .await
            .unwrap();
        assert_eq!(after.refresh().await.unwrap(), ["krabby", "nipper"]);
        let row = store.workers().get("krabby").await.unwrap().unwrap();
        assert_eq!(row.health, "healthy");
        assert_eq!(row.capabilities["tools"], serde_json::json!(["caldav"]));
        assert!(row.last_seen.is_some());

        assert!(after.remove("krabby").await.unwrap());
        assert!(matches!(
            store.secrets().get(&token_secret("krabby")).await,
            Err(Error::NotFound(_))
        ));
    }

    /// What a local worker's check of its provider's model found.
    enum ModelCheck {
        Present,
        Missing { base_url: String, model: String },
    }

    struct Local {
        name: String,
        check: ModelCheck,
    }

    #[async_trait]
    impl Worker for Local {
        fn name(&self) -> &str {
            &self.name
        }
        fn kind(&self) -> WorkerKind {
            WorkerKind::Local
        }
        fn capabilities(&self) -> WorkerCapabilities {
            WorkerCapabilities::default()
        }
        fn healthy(&self) -> bool {
            matches!(self.check, ModelCheck::Present)
        }
        fn unhealthy_reason(&self) -> Option<String> {
            match &self.check {
                ModelCheck::Present => None,
                ModelCheck::Missing { base_url, model } => {
                    Some(format!("Ollama at {base_url} has no model `{model}`"))
                }
            }
        }
        async fn run(&self, _brief: Brief) -> Result<ResultReport, Error> {
            Ok(ResultReport::default())
        }
    }

    #[tokio::test]
    async fn an_unhealthy_worker_shows_why_on_its_row_and_view() {
        let (_dir, store) = temp_store();
        let registry = WorkerRegistry::new(store.clone());
        let registered = registry
            .register(
                Arc::new(Local {
                    name: "snapper".to_string(),
                    check: ModelCheck::Missing {
                        base_url: "http://127.0.0.1:11434".to_string(),
                        model: "qwen3-coder:30b".to_string(),
                    },
                }),
                serde_json::json!({}),
                None,
            )
            .await
            .unwrap();
        let want = "unhealthy: Ollama at http://127.0.0.1:11434 has no model `qwen3-coder:30b`";
        assert!(!registered.healthy);
        assert_eq!(registered.health, want);
        let row = store.workers().get("snapper").await.unwrap().unwrap();
        assert_eq!(row.health, want);
        let view = registry.view("snapper").await.unwrap().unwrap();
        assert!(view.health.contains("qwen3-coder:30b"), "{}", view.health);
        assert!(view.last_seen.is_none());

        // A worker with no reason to give keeps the bare line; a healthy one
        // gives none.
        let fixed = WorkerRegistry::fixed(
            store.clone(),
            vec![
                Arc::new(Named {
                    name: "krabby".to_string(),
                    kind: WorkerKind::Peer,
                    repos: Vec::new(),
                    token: None,
                    refreshes: std::sync::atomic::AtomicUsize::new(0),
                }),
                Arc::new(Local {
                    name: "pinch".to_string(),
                    check: ModelCheck::Present,
                }),
            ],
        );
        let views = fixed.views().await.unwrap();
        let health = |name: &str| {
            views
                .iter()
                .find(|v| v.name == name)
                .map(|v| v.health.clone())
                .unwrap()
        };
        assert_eq!(health("krabby"), "unhealthy");
        assert_eq!(health("pinch"), "healthy");
    }

    #[test]
    fn worker_names_are_checked() {
        for good in ["pinch", "pinch-s17", "a_b", "K9"] {
            assert!(valid_name(good), "{good}");
        }
        for bad in ["", "9lives", "a b", "a/b", &"x".repeat(33)] {
            assert!(!valid_name(bad), "{bad}");
        }
    }
    #[tokio::test]
    async fn observing_workers_does_not_advance_their_probe_time() {
        let (_dir, store) = temp_store();
        let registry = WorkerRegistry::new(store.clone()).with_factory(Arc::new(Factory));
        registry
            .add(spec(WorkerKind::ClaudeCode, Some("pinch")))
            .await
            .unwrap();
        let before = store.workers().get("pinch").await.unwrap().unwrap();
        registry.views().await.unwrap();
        registry.view("pinch").await.unwrap();
        let after = store.workers().get("pinch").await.unwrap().unwrap();
        assert_eq!(after.last_seen, before.last_seen);
        assert_eq!(after.health, before.health);
    }

    #[tokio::test]
    async fn a_standalone_planner_does_not_take_the_local_execution_name() {
        let (_dir, store) = temp_store();
        let registry = WorkerRegistry::new(store.clone());
        registry
            .register(
                Arc::new(Named {
                    name: "planner".into(),
                    kind: WorkerKind::Local,
                    repos: Vec::new(),
                    token: None,
                    refreshes: std::sync::atomic::AtomicUsize::new(0),
                }),
                serde_json::json!({"role":"planner"}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(registry.local_name().await.unwrap(), "snapper");
        let restarted = WorkerRegistry::new(store);
        assert_eq!(restarted.local_name().await.unwrap(), "snapper");
    }
}
