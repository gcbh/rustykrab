//! Functional capability proof, the controller's side
//! (`docs/verification/capability-proof-2026-10-14.md`). The pure validator is
//! [`crate::capability_proof`]; the observations are the workers'
//! ([`Worker::probe`]).
//!
//! A capability item for a missing tool used to close `done` once the host
//! catalog named the tool, and closing it revived the item that needed the
//! tool without anyone checking that the operation now worked: on v6.5.6 one
//! item ran 491 times that way. For a need with a registered probe
//! ([`registered_action`]: the host binaries `cargo` and `node`, the
//! RustyKrab tool `work_resources`) the controller now asks the worker that
//! failed the waiting item to perform that operation itself, and judges what
//! the worker observed with [`capability_proven`]:
//!
//! 1. **The expected operation** comes from controller state alone. The
//!    executor is the stable name of the worker the waiting item's last
//!    lease went to; the action is the registered one, with exactly its
//!    registered arguments; the scope is that worker's
//!    [`Worker::probe_scope`] for the waiting item's run directory (a fresh
//!    worktree at the base its last run recorded, when it is
//!    repository-scoped, removed after the probe); the inputs digest is
//!    [`probe_inputs_digest`]; the generation is `<waiting item>:<n>`; the
//!    run is a fresh probe id.
//! 2. **The generation is written first.** Before a probe is dispatched the
//!    controller writes a [`REPAIR_GENERATION`] row on the waiting item
//!    ([`RepairGeneration`]): `n`, its cause, the capability item that
//!    caused it, the executor's effective-access digest and the bindings of
//!    the probes that judge it. Every registered need of the waiting item
//!    whose capability is done is probed again at the new generation.
//! 3. **Dispatch** uses the executor's own capacity, never a second
//!    scheduler: a done claim waits in the reconcile queue until the
//!    executor has a free slot, as a lease would ([`Controller::probe_waits`]),
//!    the probe holds that slot while it runs, and it is bounded by
//!    [`super::ControllerConfig::probe_deadline`] (at most 30 seconds).
//! 4. **The receipt** is what [`Worker::probe`] returned, and only that:
//!    an `operation_probe` row on the capability item whose `hash` is the
//!    expected inputs digest and whose `verified_by` is
//!    `executor:<worker>`. A report cannot store one
//!    (`breaker::CONTROLLER_KINDS`).
//! 5. **Gate 1** ([`Controller::judge_capability`]): such a capability item
//!    closes `done` only on `Proof` for every item waiting on it. A failed
//!    or unobserved probe fails its verification with the observation as
//!    the detail, so it is neither `done` nor landed, and the ladder and the
//!    repeated-outcome breaker bound its repairs.
//! 6. **Gate 2** ([`Controller::revival_proven`]): an item parked behind
//!    such a capability resumes only when every one of its registered needs
//!    has `Proof` for its current generation, in addition to #693's
//!    freshness rule (`filing::parked_on_landed_capability`).
//!
//! Catalog presence never becomes proof. A need without a registered probe
//! keeps the catalog check for a build and nothing for an acquisition, and
//! says so: its `done` is labelled availability only.

use std::sync::Arc;
use std::time::Duration;

use rustykrab_core::work::{EdgeKind, EventKind, Evidence, Status, WorkEvent, WorkItem, WorkKind};
use rustykrab_core::Error;
use rustykrab_tools::work_backend::ToolState;
use serde::{Deserialize, Serialize};

use crate::capability_proof::{
    capability_proven, probe_inputs_digest, ExpectedOperation, Missing, OperationProbe,
    ProbeRequest, RegisteredAction, OPERATION_PROBE,
};
use crate::errors::{GapKind, VerifierVerdict};
use crate::graph::Snapshot;
use crate::routing::{built_tool, CapabilityRef};
use crate::worker::Worker;
use crate::workspace::Workspace;

use super::notice::label;
use super::Controller;

/// The evidence kind of a waiting item's repair generation, written by the
/// controller before it dispatches the probes that judge it.
pub(super) const REPAIR_GENERATION: &str = "repair_generation";

/// Who writes a [`REPAIR_GENERATION`] row.
const BY_CONTROLLER: &str = "controller";

/// The longest a probe may take ([`ProbeRequest::valid`] refuses more).
pub(super) const PROBE_DEADLINE_CAP: Duration = Duration::from_secs(30);

/// How long past its own deadline the controller waits for a worker's
/// receipt before it gives up on the probe without one.
const RECEIPT_GRACE: Duration = Duration::from_secs(1);

/// Who verified a built tool's presence: the host's catalog.
const BY_CATALOG: &str = "catalog";

/// The registered probe for a capability need, if it has one: a host binary
/// Codex's producer registered (`cargo`, `node`) is checked with
/// `BinaryAvailable`; a RustyKrab tool with a registered read-only probe
/// (`work_resources`) with `ToolCallable` and exactly its registered
/// arguments. Anything else has no probe and stays availability only.
///
/// This mirrors the producer's registry in `rustykrab-agent`'s
/// `capability_probe.rs`, which refuses (`Unobserved`) anything it does not
/// register, so a mismatch fails closed.
pub(crate) fn registered_action(need: &CapabilityRef) -> Option<RegisteredAction> {
    match (need.gap, need.subject.as_str()) {
        (GapKind::Tool | GapKind::Install, binary @ ("cargo" | "node")) => {
            Some(RegisteredAction::BinaryAvailable {
                binary: binary.to_string(),
            })
        }
        (GapKind::Tool, tool @ "work_resources") => Some(RegisteredAction::ToolCallable {
            tool: tool.to_string(),
            args: serde_json::json!({}),
        }),
        _ => None,
    }
}

/// A capability item's need and its registered probe, if both exist.
fn registered_need(item: &WorkItem) -> Option<(CapabilityRef, RegisteredAction)> {
    if item.kind != WorkKind::Capability {
        return None;
    }
    let need = CapabilityRef::of_refs(&item.artifact_refs)?;
    let action = registered_action(&need)?;
    Some((need, action))
}

/// A waiting item's repair generation, as its [`REPAIR_GENERATION`] row
/// records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct RepairGeneration {
    /// Counts up from 1 per waiting item.
    pub n: u64,
    /// What started it: `capability` (a capability item it waits on
    /// reported done).
    pub cause: String,
    /// The item whose completion started it.
    pub source_item: String,
    /// The executor's effective-access digest at this generation
    /// (`Controller::access_digest`); `None` when no executor is known.
    pub permission_digest: Option<String>,
    /// The probes that judge this generation, as the controller asked for
    /// them: one per registered need it could ask a probe for.
    #[serde(default)]
    pub probes: Vec<PlannedProbe>,
}

/// One probe a generation is judged by: the capability item whose operation
/// it exercises, and the bindings the controller expects of its receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct PlannedProbe {
    pub capability: String,
    pub expected: ExpectedOperation,
}

/// The generation string a probe binds to.
fn generation_of(item: &str, n: u64) -> String {
    format!("{item}:{n}")
}

/// The latest repair generation in an item's evidence: the highest `n`
/// among the controller's rows.
pub(super) fn latest_generation(evidence: &[Evidence]) -> Option<RepairGeneration> {
    evidence
        .iter()
        .filter(|e| e.kind == REPAIR_GENERATION && e.verified_by.as_deref() == Some(BY_CONTROLLER))
        .filter_map(|e| serde_json::from_str::<RepairGeneration>(&e.reference).ok())
        .max_by_key(|g| g.n)
}

/// The worker the item's last lease went to: the actor of its latest
/// `lease` event (`worker:<name>`), which only the controller writes.
pub(super) fn failing_worker(events: &[WorkEvent]) -> Option<String> {
    events
        .iter()
        .rev()
        .find(|e| e.kind == EventKind::Lease)
        .map(|e| {
            e.actor
                .strip_prefix("worker:")
                .unwrap_or(&e.actor)
                .to_string()
        })
        .filter(|w| !w.is_empty())
}

/// The open items parked behind capability item `cap` through a `blocks`
/// edge: whose operation its probes are for.
fn waiters<'a>(snap: &'a Snapshot, cap: &str) -> Vec<&'a WorkItem> {
    let mut out: Vec<&WorkItem> = snap
        .edges_naming(cap)
        .filter(|e| e.kind == EdgeKind::Blocks)
        .filter_map(|e| snap.item(&e.item))
        .filter(|w| !w.status.is_closed())
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out.dedup_by(|a, b| a.id == b.id);
    out
}

/// The registered needs of `item`: each capability item it holds a `blocks`
/// edge on whose need has a registered probe, with that probe's action.
fn registered_needs(snap: &Snapshot, item: &str) -> Vec<(WorkItem, RegisteredAction)> {
    let mut out: Vec<(WorkItem, RegisteredAction)> = snap
        .edges_held_by(item)
        .filter(|e| e.kind == EdgeKind::Blocks)
        .filter_map(|e| snap.item(&e.depends_on))
        .filter_map(|c| registered_need(c).map(|(_, action)| (c.clone(), action)))
        .collect();
    out.sort_by(|a, b| a.0.id.cmp(&b.0.id));
    out.dedup_by(|a, b| a.0.id == b.0.id);
    out
}

/// Whether `item` waits on a capability whose need has a registered probe.
pub(super) fn has_registered_need(snap: &Snapshot, item: &str) -> bool {
    !registered_needs(snap, item).is_empty()
}

/// The `operation_probe` rows of a capability item's evidence that can
/// speak for `generation`: those bound to it, and any the controller could
/// not read (which then fail the proof closed rather than vanish). The
/// validator's latest-row rule applies among them, so a later failed or
/// unobserved receipt for the same generation is never hidden behind an
/// earlier success; receipts for another waiting item or another
/// generation of this one do not compete.
fn rows_for(evidence: &[Evidence], generation: &str) -> Vec<Evidence> {
    evidence
        .iter()
        .filter(|e| e.kind == OPERATION_PROBE)
        .filter(
            |e| match serde_json::from_str::<OperationProbe>(&e.reference) {
                Ok(p) => p.generation == generation,
                Err(_) => true,
            },
        )
        .cloned()
        .collect()
}

/// Why a registered need is not proven, as a verifier verdict and a
/// sentence with nothing run-specific in it (the failure's fingerprint is
/// cut from it, and the breaker compares fingerprints).
fn unproven(missing: &Missing, probe: Option<&OperationProbe>) -> (VerifierVerdict, String) {
    let verdict = match missing {
        Missing::Failed => VerifierVerdict::ClaimMismatch,
        _ => VerifierVerdict::Incomplete,
    };
    let observed = probe
        .map(|p| format!("; the receipt says: {}", p.observation.trim()))
        .unwrap_or_default();
    (verdict, format!("{missing}{observed}"))
}

/// What gate 1 found for a capability item's done claim.
pub(super) enum CapabilityVerdict {
    /// Every waiting item's registered need has `Proof`; the operations.
    Proven(Vec<String>),
    /// No registered probe: today's availability check passed. The proof
    /// rows it adds (the catalog's word for a built tool).
    AvailabilityOnly(Vec<(String, String, Option<String>)>),
    /// Not proven, or not available: the verdict and why.
    Unproven(VerifierVerdict, String),
}

/// One waiting item's planned probe of one registered need.
struct Plan {
    capability: WorkItem,
    request: Result<ProbeRequest, String>,
}

/// A slot of the executor's capacity, held while its probe runs and given
/// back when dropped.
struct ProbeSlot<'a> {
    ctl: &'a Controller,
    worker: String,
}

impl<'a> ProbeSlot<'a> {
    fn take(ctl: &'a Controller, worker: &str) -> ProbeSlot<'a> {
        *ctl.state().probing.entry(worker.to_string()).or_default() += 1;
        ProbeSlot {
            ctl,
            worker: worker.to_string(),
        }
    }
}

impl Drop for ProbeSlot<'_> {
    fn drop(&mut self) {
        let mut state = self.ctl.state();
        if let Some(n) = state.probing.get_mut(&self.worker) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                state.probing.remove(&self.worker);
            }
        }
    }
}

impl Controller {
    /// Gate 1, for a capability item that claims done: [`CapabilityVerdict`].
    ///
    /// A need with a registered probe is judged by the probes of every item
    /// waiting on it (see the module docs); one with none keeps today's
    /// check, availability only: a build of a tool must have put the tool in
    /// the host's catalog, and an acquisition is not checked.
    pub(super) async fn judge_capability(
        &self,
        snap: &Snapshot,
        item: &WorkItem,
    ) -> Result<CapabilityVerdict, Error> {
        let Some((need, action)) = registered_need(item) else {
            return self.availability(item).await;
        };
        let waiting = waiters(snap, &item.id);
        if waiting.is_empty() {
            return Ok(CapabilityVerdict::Unproven(
                VerifierVerdict::Incomplete,
                format!(
                    "the {} `{}` has a registered probe ({}), but no item waits on it to \
                     name the executor to probe on",
                    need.gap.as_str(),
                    need.subject,
                    action.operation()
                ),
            ));
        }
        for w in waiting {
            if let Err((verdict, why)) = self.probe_waiter(snap, w, item).await? {
                return Ok(CapabilityVerdict::Unproven(
                    verdict,
                    format!(
                        "the {} `{}` is not proven for {}: {} {why}",
                        need.gap.as_str(),
                        need.subject,
                        label(w),
                        action.operation()
                    ),
                ));
            }
        }
        Ok(CapabilityVerdict::Proven(vec![action.operation()]))
    }

    /// Today's check for a need without a registered probe.
    async fn availability(&self, item: &WorkItem) -> Result<CapabilityVerdict, Error> {
        let mode = self.capability_mode(&item.id).await?;
        let mut proofs = Vec::new();
        if let Some(tool) = built_tool(mode, &item.artifact_refs) {
            self.catalog.refresh();
            if self.catalog.tool_state(&tool) == ToolState::Unknown {
                return Ok(CapabilityVerdict::Unproven(
                    VerifierVerdict::ClaimMismatch,
                    format!(
                        "the result says the tool is built, but the host has no tool named \
                         `{tool}`"
                    ),
                ));
            }
            proofs.push(("tool".to_string(), tool, Some(BY_CATALOG.to_string())));
        }
        Ok(CapabilityVerdict::AvailabilityOnly(proofs))
    }

    /// Open a new repair generation on waiting item `w` because `cap`
    /// claims done, probe each of `w`'s registered needs whose capability is
    /// `cap` or already done, and say whether `cap`'s own need is proven.
    async fn probe_waiter(
        &self,
        snap: &Snapshot,
        w: &WorkItem,
        cap: &WorkItem,
    ) -> Result<Result<(), (VerifierVerdict, String)>, Error> {
        let needs: Vec<(WorkItem, RegisteredAction)> = registered_needs(snap, &w.id)
            .into_iter()
            .filter(|(c, _)| c.id == cap.id || c.status == Status::Done)
            .collect();
        let events = self.store.work_events(&w.id).await?;
        let evidence = self.store.work_evidence_list(&w.id).await?;
        let n = latest_generation(&evidence).map_or(1, |g| g.n + 1);
        let generation = generation_of(&w.id, n);
        let executor = failing_worker(&events);
        let worker = executor.as_deref().and_then(|e| self.worker(e));

        // Where a run of the waiting item would run: a fresh worktree for a
        // repository-scoped one, as its adapter would be given.
        let probe_id = uuid::Uuid::new_v4().to_string();
        let workspace = match &worker {
            Some(_) => self.probe_workspace(w, &probe_id).await,
            None => Ok(None),
        };
        let scope = match (&worker, &workspace) {
            (None, _) => Err(match &executor {
                Some(name) => format!("on {name}: that worker is not registered"),
                None => "has no executor: no lease is on record for the waiting item".to_string(),
            }),
            (Some(worker), Err(why)) => Err(format!(
                "on {}: its run directory could not be prepared: {why}",
                worker.name()
            )),
            (Some(worker), Ok(ws)) => worker
                .probe_scope(ws.as_ref().map(|ws| ws.path.clone()))
                .filter(|s| s.worker == worker.name())
                .ok_or_else(|| {
                    format!(
                        "on {}: that worker cannot run registered probes (it offers no probe \
                         scope)",
                        worker.name()
                    )
                }),
        };
        let deadline = self
            .config
            .probe_deadline
            .clamp(Duration::from_millis(1), PROBE_DEADLINE_CAP);
        let plans: Vec<Plan> = needs
            .into_iter()
            .map(|(capability, action)| {
                let request = scope.clone().and_then(|scope| {
                    let expected = ExpectedOperation {
                        operation: action.operation(),
                        executor: scope.worker.clone(),
                        inputs_digest: probe_inputs_digest(&action, &scope),
                        generation: generation.clone(),
                        run: format!("probe-{}", uuid::Uuid::new_v4()),
                    };
                    let request = ProbeRequest {
                        expected,
                        action,
                        scope,
                        deadline,
                    };
                    if request.valid() {
                        Ok(request)
                    } else {
                        Err(format!(
                            "on {}: the probe request it would get is not valid",
                            request.scope.worker
                        ))
                    }
                });
                Plan {
                    capability,
                    request,
                }
            })
            .collect();

        // The generation is on record before anything is dispatched.
        let permission_digest = match &worker {
            Some(worker) => Some(self.access_digest(worker.as_ref()).await),
            None => None,
        };
        let record = RepairGeneration {
            n,
            cause: "capability".to_string(),
            source_item: cap.id.clone(),
            permission_digest,
            probes: plans
                .iter()
                .filter_map(|p| {
                    p.request.as_ref().ok().map(|r| PlannedProbe {
                        capability: p.capability.id.clone(),
                        expected: r.expected.clone(),
                    })
                })
                .collect(),
        };
        self.store
            .work_evidence_add(Evidence {
                item: w.id.clone(),
                kind: REPAIR_GENERATION.to_string(),
                reference: serde_json::to_string(&record)
                    .map_err(|e| Error::Internal(format!("repair generation: {e}")))?,
                hash: Some(generation.clone()),
                verified_by: Some(BY_CONTROLLER.to_string()),
                at: self.clock.now(),
            })
            .await?;

        let observed = self
            .observe_all(worker.as_ref(), plans, cap, &generation)
            .await;
        // The probe's worktree goes whatever the probes found.
        if let Ok(Some(ws)) = workspace {
            discard(ws).await;
        }
        Ok(observed?.unwrap_or_else(|| {
            Err((
                VerifierVerdict::Incomplete,
                "was not probed: the waiting item does not hold its edge any more".to_string(),
            ))
        }))
    }

    /// Dispatch every planned probe in turn; `cap`'s own outcome, if it had
    /// a plan.
    async fn observe_all(
        &self,
        worker: Option<&Arc<dyn Worker>>,
        plans: Vec<Plan>,
        cap: &WorkItem,
        generation: &str,
    ) -> Result<Option<Result<(), (VerifierVerdict, String)>>, Error> {
        let mut own = None;
        for plan in plans {
            let outcome = match (worker, plan.request) {
                (Some(worker), Ok(request)) => {
                    self.observe(worker, &plan.capability, &request, generation)
                        .await?
                }
                (_, Err(why)) => Err((VerifierVerdict::Incomplete, why)),
                (None, Ok(_)) => Err((VerifierVerdict::Incomplete, "has no executor".to_string())),
            };
            if plan.capability.id == cap.id {
                own = Some(outcome);
            }
        }
        Ok(own)
    }

    /// Dispatch one probe on `worker` within its capacity and its deadline,
    /// store the receipt it returns on `capability`, and judge it.
    async fn observe(
        &self,
        worker: &Arc<dyn Worker>,
        capability: &WorkItem,
        request: &ProbeRequest,
        generation: &str,
    ) -> Result<Result<(), (VerifierVerdict, String)>, Error> {
        let observed = {
            let _slot = ProbeSlot::take(self, worker.name());
            tokio::time::timeout(request.deadline + RECEIPT_GRACE, worker.probe(request)).await
        };
        let Ok(probe) = observed else {
            tracing::warn!(item = %capability.id, worker = %worker.name(), "probe returned no receipt within its deadline");
            return Ok(Err((
                VerifierVerdict::Incomplete,
                format!(
                    "on {}: the probe returned no receipt within its deadline",
                    worker.name()
                ),
            )));
        };
        self.store
            .work_evidence_add(Evidence {
                item: capability.id.clone(),
                kind: OPERATION_PROBE.to_string(),
                reference: serde_json::to_string(&probe)
                    .map_err(|e| Error::Internal(format!("operation probe: {e}")))?,
                hash: Some(request.expected.inputs_digest.clone()),
                verified_by: Some(format!("executor:{}", request.expected.executor)),
                at: self.clock.now(),
            })
            .await?;
        let rows = rows_for(
            &self.store.work_evidence_list(&capability.id).await?,
            generation,
        );
        Ok(
            match capability_proven(capability, &rows, &request.expected) {
                Ok(_) => Ok(()),
                Err(missing) => {
                    let (verdict, why) = unproven(&missing, Some(&probe));
                    Err((verdict, format!("on {}: {why}", worker.name())))
                }
            },
        )
    }

    /// Gate 2: whether `item`, parked behind capability items, may resume.
    /// Every one of its registered needs must have `Proof` for its current
    /// generation, bound as that generation's row recorded it. An item whose
    /// needs are all availability only may (today's rule).
    pub(super) async fn revival_proven(&self, snap: &Snapshot, item: &str) -> Result<bool, Error> {
        let needs = registered_needs(snap, item);
        if needs.is_empty() {
            return Ok(true);
        }
        let evidence = self.store.work_evidence_list(item).await?;
        let Some(current) = latest_generation(&evidence) else {
            return Ok(false);
        };
        let generation = generation_of(item, current.n);
        for (capability, action) in needs {
            let Some(planned) = current
                .probes
                .iter()
                .find(|p| p.capability == capability.id)
            else {
                return Ok(false);
            };
            if planned.expected.generation != generation
                || planned.expected.operation != action.operation()
            {
                return Ok(false);
            }
            let rows = rows_for(
                &self.store.work_evidence_list(&capability.id).await?,
                &generation,
            );
            if capability_proven(&capability, &rows, &planned.expected).is_err() {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Whether a finished capability item's done claim has to wait for its
    /// probes' executors to have a free slot, as a lease waits for one. A
    /// probe takes one of its executor's slots; a done claim is judged once
    /// every executor it needs can take one. An executor that is not
    /// registered does not hold it up: its need cannot be proven, and gate 1
    /// says so.
    pub(super) async fn probe_waits(
        &self,
        snap: &Snapshot,
        id: &str,
        result: &rustykrab_core::work::ResultReport,
    ) -> Result<bool, Error> {
        if result.error.is_some() || result.blocked.is_some() || result.summary.trim().is_empty() {
            return Ok(false);
        }
        let Some(item) = snap.item(id) else {
            return Ok(false);
        };
        if registered_need(item).is_none() {
            return Ok(false);
        }
        for w in waiters(snap, id) {
            let events = self.store.work_events(&w.id).await?;
            let Some(worker) = failing_worker(&events).and_then(|name| self.worker(&name)) else {
                continue;
            };
            if !self.dispatch_available(worker.as_ref()) || !self.has_free_slot(worker.as_ref()) {
                tracing::debug!(item = %id, worker = %worker.name(), "capability probe waits for a free slot");
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// A fresh worktree for a probe of repository-scoped item `w`, at the
    /// base its last run recorded: the run directory a run of it would get.
    /// `None` for an item with no recorded workspace or when no worktree
    /// root is configured.
    async fn probe_workspace(&self, w: &WorkItem, run: &str) -> Result<Option<Workspace>, String> {
        let Some(root) = self.config.worktree_root.clone() else {
            return Ok(None);
        };
        let Some(recorded) = self.workspace_of(&w.id).await.map_err(|e| e.to_string())? else {
            return Ok(None);
        };
        let ws = Workspace::plan(&root, &recorded.repo, &recorded.base, &w.id, run);
        let create = ws.clone();
        tokio::task::spawn_blocking(move || create.create())
            .await
            .map_err(|e| format!("worktree task failed: {e}"))??;
        Ok(Some(ws))
    }
}

/// Remove a probe's worktree and its branch: a probe commits nothing.
async fn discard(ws: Workspace) {
    let done = tokio::task::spawn_blocking(move || {
        let removed = ws.remove();
        let branch = std::process::Command::new("git")
            .arg("-C")
            .arg(&ws.repo)
            .args(["branch", "-D", &ws.branch])
            .output();
        (removed, branch.map(|o| o.status.success()))
    })
    .await;
    match done {
        Ok((Ok(()), Ok(true))) => {}
        other => tracing::warn!(?other, "probe worktree not fully removed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn need(gap: GapKind, subject: &str) -> CapabilityRef {
        CapabilityRef {
            gap,
            subject: subject.to_string(),
        }
    }

    #[test]
    fn only_registered_needs_get_a_probe_with_exactly_its_registered_arguments() {
        assert_eq!(
            registered_action(&need(GapKind::Tool, "cargo")),
            Some(RegisteredAction::BinaryAvailable {
                binary: "cargo".into()
            })
        );
        assert_eq!(
            registered_action(&need(GapKind::Install, "node")),
            Some(RegisteredAction::BinaryAvailable {
                binary: "node".into()
            })
        );
        assert_eq!(
            registered_action(&need(GapKind::Tool, "work_resources")),
            Some(RegisteredAction::ToolCallable {
                tool: "work_resources".into(),
                args: serde_json::json!({}),
            })
        );
        for (gap, subject) in [
            (GapKind::Tool, "pdf_render"),
            (GapKind::Tool, "python3"),
            (GapKind::Install, "work_resources"),
            (GapKind::Credential, "cargo"),
            (GapKind::Tool, "Cargo"),
        ] {
            assert_eq!(registered_action(&need(gap, subject)), None, "{subject}");
        }
    }

    #[test]
    fn the_latest_generation_is_the_controllers_highest() {
        let row = |n: u64, by: Option<&str>| Evidence {
            item: "w".into(),
            kind: REPAIR_GENERATION.into(),
            reference: serde_json::to_string(&RepairGeneration {
                n,
                cause: "capability".into(),
                source_item: "c".into(),
                permission_digest: None,
                probes: vec![],
            })
            .unwrap(),
            hash: Some(generation_of("w", n)),
            verified_by: by.map(str::to_string),
            at: chrono::Utc::now(),
        };
        let rows = vec![
            row(2, Some(BY_CONTROLLER)),
            row(9, None),
            row(1, Some(BY_CONTROLLER)),
        ];
        assert_eq!(latest_generation(&rows).map(|g| g.n), Some(2));
        assert_eq!(latest_generation(&[]), None);
    }
}
