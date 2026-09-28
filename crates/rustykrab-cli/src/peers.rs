//! Peers over the tailnet as the daemon wires them
//! (`docs/plans/control-layer-and-worker-fleet.md`, sections 5, 12.1 and 13,
//! Phase 5). Both halves of the `peer` worker kind live in one daemon:
//!
//! - **as a controller**, a peer is a worker in the registry: `worker add
//!   peer` (over `POST /api/workers`, a `base_url` and a `token` or a
//!   `pairing_code`) builds a [`PeerWorker`] through the fleet's factory
//!   ([`build`], [`pair`]), and [`spawn_refresh`] asks every worker where
//!   it stands on a timer, which records a peer's advertisement and health
//!   on its row;
//! - **as a node**, [`delegated_runs`] is how this daemon runs a peer's
//!   structured brief: a local worker per task, inside the delegation
//!   ceiling the gateway computes, sharing the local worker's model slot,
//!   its runs kept as conversations.
//!
//! Configuration, all optional: `RUSTYKRAB_MACHINE_NAME` (the machine this
//! node advertises, else the host name), `RUSTYKRAB_DELEGATION_RESOURCES`
//! (the writable resources a delegated run may claim, comma separated; none
//! by default), and the existing `RUSTYKRAB_DELEGATION_TOOLS` ceiling.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rustykrab_agent::{
    redeem_pairing_code, DelegatedRuns, LateTools, PeerConfig, PeerWorker, RunTranscripts,
};
use rustykrab_control::registry::{WorkerRegistry, WorkerSpec};
use rustykrab_control::worker::Worker;
use rustykrab_core::model::ModelProvider;
use rustykrab_core::types::Conversation;
use rustykrab_core::{AgentDefinition, Tool};
use rustykrab_store::Store;
use tokio::sync::Semaphore;

/// How often the registry asks its workers where they stand. A peer
/// re-reads its node's advertisement at most every 30 s while the node
/// answers, and on every tick while it does not, so a node that comes up is
/// leasable within a tick or two.
const REFRESH_TICK: Duration = Duration::from_secs(2);

/// The machine this daemon names when it advertises itself as a node, and
/// the device name it pairs under as a controller.
pub(crate) fn machine_name() -> Option<String> {
    if let Some(name) = std::env::var("RUSTYKRAB_MACHINE_NAME")
        .ok()
        .filter(|n| !n.trim().is_empty())
    {
        return Some(name.trim().to_string());
    }
    let out = std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .ok()?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// The writable resources a delegated run on this node may claim.
fn delegation_resources() -> Vec<String> {
    std::env::var("RUSTYKRAB_DELEGATION_RESOURCES")
        .map(|raw| {
            raw.split(',')
                .map(|r| r.trim().to_string())
                .filter(|r| !r.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Build the peer a registry spec describes.
pub(crate) fn build(name: &str, spec: &WorkerSpec) -> Result<Arc<dyn Worker>, String> {
    let config = PeerConfig::from_spec(spec)?;
    Ok(Arc::new(PeerWorker::new(name, config)))
}

/// Redeem a peer spec's pairing code at its node for a device token of this
/// daemon's own, named after this machine and the worker.
pub(crate) async fn pair(name: &str, mut spec: WorkerSpec) -> Result<WorkerSpec, String> {
    let (Some(base), Some(code)) = (spec.base_url.clone(), spec.pairing_code.clone()) else {
        return Ok(spec);
    };
    let device = format!(
        "{}-controller-{name}",
        machine_name().unwrap_or_else(|| "rustykrab".to_string())
    );
    let paired = redeem_pairing_code(&base, &code, &device).await?;
    tracing::info!(worker = %name, device = %device, "paired with a peer node");
    spec.token = Some(paired.device_token);
    spec.pairing_code = None;
    Ok(spec)
}

/// Ask the registry's workers where they stand every [`REFRESH_TICK`],
/// recording what a peer advertises and its health on its row.
pub(crate) fn spawn_refresh(registry: Arc<WorkerRegistry>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(REFRESH_TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            match registry.refresh().await {
                Ok(recorded) if !recorded.is_empty() => {
                    tracing::debug!(workers = ?recorded, "worker advertisements refreshed");
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "worker refresh failed"),
            }
        }
    })
}

/// Keeps a delegated run's conversation in this node's store, so the run
/// is a conversation like any other; never continues a kept one.
struct NodeTranscripts {
    store: Store,
}

#[async_trait]
impl RunTranscripts for NodeTranscripts {
    async fn save(&self, conversation: &Conversation) -> rustykrab_core::Result<()> {
        self.store.conversations().save(conversation).await
    }
}

/// What a node needs to run a peer's brief.
pub(crate) struct NodeParts {
    pub name: String,
    pub definition: AgentDefinition,
    pub provider: Arc<dyn ModelProvider>,
    pub tools: Vec<Arc<dyn Tool>>,
    pub store: Store,
    pub late: Arc<dyn LateTools>,
    /// The local worker's model slot, shared.
    pub slot: Arc<Semaphore>,
}

/// How this daemon runs a peer's structured brief, for the gateway's task
/// worker and `GET /api/node`.
pub(crate) fn delegated_runs(parts: NodeParts) -> Arc<DelegatedRuns> {
    Arc::new(
        DelegatedRuns::new(
            parts.name,
            parts.definition,
            parts.provider,
            parts.tools,
            Arc::new(rustykrab_agent::ProcessSandbox::new()),
        )
        .with_transcripts(Arc::new(NodeTranscripts { store: parts.store }))
        .with_late_tools(parts.late)
        .with_slot(parts.slot)
        .with_machine(machine_name())
        .with_writable_resources(delegation_resources()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustykrab_core::work::WorkerKind;

    #[test]
    fn a_peer_is_built_from_its_spec_and_refused_without_a_token() {
        let mut spec = WorkerSpec {
            kind: WorkerKind::Peer,
            base_url: Some("http://127.0.0.1:3100".into()),
            ..WorkerSpec::default()
        };
        assert!(build("krabby", &spec).is_err());
        spec.token = Some("t".into());
        let worker = build("krabby", &spec).unwrap();
        assert_eq!(worker.name(), "krabby");
        assert_eq!(worker.kind(), WorkerKind::Peer);
        assert!(!worker.healthy(), "not until its node answers");
    }

    #[tokio::test]
    async fn a_spec_without_a_pairing_code_is_left_alone() {
        let spec = WorkerSpec {
            kind: WorkerKind::Peer,
            base_url: Some("http://127.0.0.1:9".into()),
            token: Some("t".into()),
            ..WorkerSpec::default()
        };
        let out = pair("krabby", spec.clone()).await.unwrap();
        assert_eq!(out.token, spec.token);
    }
}
