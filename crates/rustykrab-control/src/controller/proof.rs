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

pub(crate) use crate::capability_proof::registered_action;
use crate::capability_proof::{
    capability_proven_in_generation, probe_inputs_digest, ExpectedOperation, Missing,
    OperationProbe, ProbeContext, ProbeRequest, ProbeScope, RegisteredAction, WorkspaceIdentity,
    OPERATION_PROBE,
};
use crate::errors::VerifierVerdict;
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
    /// None on legacy rows. Some also records an unavailable executor/scope
    /// explicitly, so unchanged unknowns do not generate a probe every tick.
    #[serde(default)]
    pub binding: Option<RepairBinding>,
    /// The probes that judge this generation, as the controller asked for
    /// them: one per registered need it could ask a probe for.
    #[serde(default)]
    pub probes: Vec<PlannedProbe>,
}

/// Stable selection observed by the controller. A transient probe directory,
/// receipt outcome, run id, health or available slot is never identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct RepairBinding {
    pub executor: Option<String>,
    pub scope: Option<ProbeScope>,
    pub context_digest: String,
    /// Trusted landed internal prerequisites, not model repair claims or time.
    pub repairs_digest: String,
    /// The controller's actual configured build identity, stable across ticks.
    pub daemon_build: String,
}

/// One probe a generation is judged by: the capability item whose operation
/// it exercises, and the bindings the controller expects of its receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct PlannedProbe {
    pub capability: String,
    pub expected: ExpectedOperation,
}

/// Only diagnoses this controller recorded allocating for this waiter may
/// trigger a repair generation. Unrelated internal prerequisites are not repair
/// signals. This is an existing rung record, not a second incident ledger.
fn revival_investigations(item: &str, events: &[WorkEvent]) -> std::collections::BTreeSet<String> {
    let prefix = format!("capability revival action {item}:");
    events
        .iter()
        .filter(|e| e.actor == BY_CONTROLLER)
        .filter_map(super::load::decode_rung)
        .filter(|r| r.rung == rustykrab_core::work::Rung::Improve && r.outcome.starts_with(&prefix))
        .filter_map(|r| {
            r.outcome
                .split_once("; investigation ")
                .and_then(|(_, tail)| tail.split_once(';'))
                .map(|(id, _)| id.to_string())
        })
        .collect()
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

    /// Reuse the real brief's post-activatable requirements. Planning a scope
    /// does not allocate a worktree; only actual dispatch does.
    pub(super) async fn repair_context(
        &self,
        w: &WorkItem,
        events: &[WorkEvent],
    ) -> Result<ProbeContext, Error> {
        let history = self.activatable(super::load::history(events));
        let mut requirements = super::brief::effective_requirements(w, &history);
        if self.config.worktree_root.is_some() {
            requirements.workspace = self.workspace_of(&w.id).await?.map(|ws| WorkspaceIdentity {
                repo: ws.repo,
                base: ws.base,
            });
        }
        Ok(ProbeContext {
            requirements,
            bound_dir: None,
        })
    }

    pub(super) async fn repair_binding(
        &self,
        snap: &Snapshot,
        item: &str,
        events: &[WorkEvent],
        context: &ProbeContext,
    ) -> Result<RepairBinding, Error> {
        let executor = failing_worker(events);
        let scope = executor
            .as_deref()
            .and_then(|e| self.worker(e))
            .and_then(|worker| {
                worker
                    .probe_scope(context)
                    .filter(|s| s.worker == worker.name())
            });
        let owned = revival_investigations(item, events);
        let mut repairs = Vec::new();
        for edge in snap
            .edges_held_by(item)
            .filter(|e| e.kind == EdgeKind::Blocks)
        {
            let Some(up) = snap.item(&edge.depends_on).filter(|up| {
                owned.contains(&up.id) && up.kind == WorkKind::Internal && up.status == Status::Done
            }) else {
                continue;
            };
            for e in self.store.work_evidence_list(&up.id).await? {
                if e.kind == "commit" && e.verified_by.as_deref() == Some("git")
                    || e.kind == crate::errors::CLASSIFIER_RULE
                        && e.verified_by.as_deref() == Some("replay")
                {
                    repairs.push((up.id.clone(), e.kind, e.reference, e.hash));
                }
            }
        }
        repairs.sort();
        repairs.dedup();
        Ok(RepairBinding {
            executor,
            scope,
            context_digest: context.requirements.digest(),
            repairs_digest: crate::capability_proof::binding_digest(&repairs),
            daemon_build: self.build_label(),
        })
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
        let mut context = self.repair_context(w, &events).await?;
        let binding = self.repair_binding(snap, &w.id, &events, &context).await?;
        let executor = &binding.executor;
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
                .probe_scope(&ProbeContext {
                    bound_dir: ws.as_ref().map(|ws| ws.path.clone()),
                    ..context.clone()
                })
                .filter(|s| s.worker == worker.name())
                .ok_or_else(|| {
                    format!(
                        "on {}: that worker cannot run registered probes (it offers no probe \
                         scope)",
                        worker.name()
                    )
                }),
        };
        context.bound_dir = workspace
            .as_ref()
            .ok()
            .and_then(|ws| ws.as_ref())
            .map(|ws| ws.path.clone());
        let scope = scope.and_then(|scope| {
            if binding.scope.as_ref() == Some(&scope) {
                Ok(scope)
            } else {
                Err("effective binding changed while preparing probe directory".into())
            }
        });
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
                        inputs_digest: probe_inputs_digest(&action, &scope, &context),
                        generation: generation.clone(),
                        run: format!("probe-{}", uuid::Uuid::new_v4()),
                    };
                    let request = ProbeRequest {
                        expected,
                        action,
                        scope,
                        context: context.clone(),
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
            binding: Some(binding),
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

        let observed = self.observe_all(worker.as_ref(), plans, cap).await;
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
    ) -> Result<Option<Result<(), (VerifierVerdict, String)>>, Error> {
        let mut own = None;
        for plan in plans {
            let outcome = match (worker, plan.request) {
                (Some(worker), Ok(request)) => {
                    self.observe(worker, &plan.capability, &request).await?
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
        let rows = self.store.work_evidence_list(&capability.id).await?;
        Ok(
            match capability_proven_in_generation(capability, &rows, &request.expected) {
                Ok(_) => Ok(()),
                Err(missing) => {
                    let (verdict, why) = unproven(&missing, Some(&probe));
                    Err((verdict, format!("on {}: {why}", worker.name())))
                }
            },
        )
    }

    /// Record a terminal refusal once per durable generation, on the original
    /// work item's existing event trail. This is an observation, not an owner
    /// question, a new attempt, a spend grant or permission to resume.
    async fn warn_revival_refusal(
        &self,
        item: &str,
        current: &RepairGeneration,
        failure: (VerifierVerdict, String),
    ) -> Result<(), Error> {
        let generation = generation_of(item, current.n);
        let reference = format!("capability_revival_blocked:{generation}");
        if self.store.work_events(item).await?.iter().any(|e| {
            e.kind == EventKind::Warning
                && e.actor == BY_CONTROLLER
                && e.evidence_ref.as_deref() == Some(reference.as_str())
        }) {
            return Ok(());
        }
        let executor = current
            .binding
            .as_ref()
            .and_then(|b| b.executor.as_deref())
            .unwrap_or("unknown");
        let path = current
            .binding
            .as_ref()
            .and_then(|b| b.scope.as_ref())
            .map(|s| format!("{:?}", s.path))
            .unwrap_or_else(|| "unknown".into());
        let reason: String = failure.1.chars().take(1024).collect();
        self.store.work_event_append(&WorkEvent {
            item: item.into(), kind: EventKind::Warning, actor: BY_CONTROLLER.into(),
            reason: Some(format!("Capability revival blocked at generation {generation}; executor {executor}; path {path}; {:?}: {reason}", failure.0)),
            evidence_ref: Some(reference), from: None, to: None, origin: None,
            upstream: Some(current.source_item.clone()), at: self.clock.now(),
        }).await?;
        Ok(())
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
        // An allocated investigation never releases the original by landing
        // alone. Wait until it is done, then bind its verified repair and probe.
        if snap.edges_held_by(item).any(|e| {
            e.kind == EdgeKind::Blocks
                && snap
                    .item(&e.depends_on)
                    .is_some_and(|up| up.kind == WorkKind::Internal && up.status != Status::Done)
        }) {
            return Ok(false);
        }
        let Some(waiter) = snap.item(item) else {
            return Ok(false);
        };
        let events = self.store.work_events(item).await?;
        let context = self.repair_context(waiter, &events).await?;
        let binding = self.repair_binding(snap, item, &events, &context).await?;
        let worker = binding.executor.as_deref().and_then(|e| self.worker(e));
        let permission_digest = match &worker {
            Some(w) => Some(self.access_digest(w.as_ref()).await),
            None => None,
        };
        let evidence = self.store.work_evidence_list(item).await?;
        let mut current = latest_generation(&evidence);
        let matches = current.as_ref().is_some_and(|g| {
            g.binding.as_ref() == Some(&binding) && g.permission_digest == permission_digest
        });
        if !matches {
            // Use the existing dispatch/capacity path; no synthetic observation
            // and no model-authored change to the item's holds or budget.
            if worker.as_ref().is_some_and(|w| {
                !self.dispatch_available(w.as_ref()) || !self.has_free_slot(w.as_ref())
            }) {
                return Ok(false);
            }
            let Some((cap, _)) = needs.iter().find(|(cap, _)| cap.status == Status::Done) else {
                return Ok(false);
            };
            let observed = self.probe_waiter(snap, waiter, cap).await?;
            current = latest_generation(&self.store.work_evidence_list(item).await?);
            if let (Some(generation), Err(failure)) = (&current, observed) {
                self.warn_revival_refusal(item, generation, failure).await?;
            }
        }
        let Some(current) = current else {
            return Ok(false);
        };
        if current.binding.as_ref() != Some(&binding)
            || current.permission_digest != permission_digest
        {
            return Ok(false);
        }
        let generation = generation_of(item, current.n);
        for (capability, action) in needs {
            let Some(planned) = current
                .probes
                .iter()
                .find(|p| p.capability == capability.id)
            else {
                self.warn_revival_refusal(
                    item,
                    &current,
                    (
                        VerifierVerdict::Incomplete,
                        format!(
                            "{} has no planned observation on the current executor",
                            action.operation()
                        ),
                    ),
                )
                .await?;
                return Ok(false);
            };
            let Some(scope) = binding.scope.as_ref() else {
                self.warn_revival_refusal(
                    item,
                    &current,
                    (
                        VerifierVerdict::Incomplete,
                        "the current executor has no observation scope".into(),
                    ),
                )
                .await?;
                return Ok(false);
            };
            if planned.expected.generation != generation
                || planned.expected.operation != action.operation()
                || planned.expected.executor != scope.worker
                || planned.expected.inputs_digest != probe_inputs_digest(&action, scope, &context)
            {
                return Ok(false);
            }
            let rows = self.store.work_evidence_list(&capability.id).await?;
            if let Err(missing) =
                capability_proven_in_generation(&capability, &rows, &planned.expected)
            {
                let (verdict, why) = unproven(&missing, None);
                self.warn_revival_refusal(
                    item,
                    &current,
                    (verdict, format!("{}: {why}", action.operation())),
                )
                .await?;
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Allocate/reuse bounded diagnosis for a terminal current-generation
    /// refusal, without changing the waiter's status or granting execution.
    /// The action marker is an ordinary controller rung, not RESOLUTION_WAIT;
    /// generic resolution recovery therefore cannot release this waiter.
    pub(super) async fn investigate_revival_refusal(
        &self,
        b: &mut super::batch::Batch,
        item: &str,
    ) -> Result<Vec<String>, Error> {
        let Some(waiter) = b.snap.item(item).cloned() else {
            return Ok(vec![]);
        };
        if waiter.held_by.is_some() {
            return Ok(vec![]);
        }
        let Some(current) = latest_generation(&self.store.work_evidence_list(item).await?) else {
            return Ok(vec![]);
        };
        let events = self.store.work_events(item).await?;
        let context = self.repair_context(&waiter, &events).await?;
        if current.binding.as_ref()
            != Some(
                &self
                    .repair_binding(&b.snap, item, &events, &context)
                    .await?,
            )
        {
            return Ok(vec![]); // A normal capacity wait for a changed selection.
        }
        let generation = generation_of(item, current.n);
        let reference = format!("capability_revival_blocked:{generation}");
        let Some(warning) = events.iter().find(|e| {
            e.kind == EventKind::Warning
                && e.actor == BY_CONTROLLER
                && e.evidence_ref.as_deref() == Some(&reference)
        }) else {
            return Ok(vec![]); // No terminal refusal observed.
        };
        let marker = format!("capability revival action {generation};");
        if events
            .iter()
            .filter_map(super::load::decode_rung)
            .any(|r| r.outcome.starts_with(&marker))
        {
            return Ok(vec![]);
        }
        let mut ladder = super::load::ladder_from(&events, &waiter);
        let mut error = crate::errors::classify(
            &crate::errors::FailureInput::Verifier {
                verdict: VerifierVerdict::Incomplete,
                detail: warning
                    .reason
                    .clone()
                    .unwrap_or_else(|| "capability proof unobserved".into()),
            },
            &crate::errors::Context {
                tool: Some("capability_probe"),
                worker_kind: current
                    .binding
                    .as_ref()
                    .and_then(|b| b.executor.as_deref())
                    .map(|worker| self.worker_kind(worker)),
            },
        );
        error.observed_by = "capability_revival".into();
        // The investigation index is the existing error-evidence ledger. This
        // records the real probe refusal, explicitly attributed to this guard;
        // it is not a failed original run, attempt record or spend charge.
        b.evidence.push(Evidence {
            item: item.into(),
            kind: super::brief::ERROR.into(),
            reference: format!(
                "{}/{}: {}",
                error.class.as_str(),
                error.subclass.as_str(),
                error.detail
            ),
            hash: Some(error.fingerprint.clone()),
            verified_by: Some(error.observed_by.clone()),
            at: b.now,
        });
        let result = if ladder.left(rustykrab_core::work::Rung::Improve) == 0 {
            Err("no remaining Improve rung for capability proof diagnosis".into())
        } else {
            let source = self.source_checkout(&[]).await;
            self.file_internal(b, &waiter, &error, source.as_ref())
        };
        let (rung, outcome, changed) = match result {
            Ok(id) => (
                rustykrab_core::work::Rung::Improve,
                format!("{marker} investigation {id}; waiter stays parked until functional Proof"),
                vec![id],
            ),
            Err(why) => {
                b.notify(item, super::notice::Cause::Asked {
                    item: item.into(),
                    text: format!("Revival remains blocked at generation {generation}: {}; no investigation could be allocated: {why}. Functional Proof is still required.", warning.reason.as_deref().unwrap_or("operation unobserved")),
                });
                (rustykrab_core::work::Rung::Surface,
                    format!("{marker} investigation unavailable: {why}; waiter stays parked until functional Proof"), vec![])
            }
        };
        // A question on NeedsTool can currently requeue on answer, bypassing
        // Proof. Keep the typed failure visible instead of creating that path.
        b.rung(
            item,
            &mut ladder,
            rustykrab_core::work::RungEvent {
                rung,
                at: b.now,
                error: Some(error),
                outcome,
            },
        );
        Ok(changed)
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
    use crate::errors::GapKind;

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
                binding: None,
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
