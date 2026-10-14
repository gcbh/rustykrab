//! The repeated-outcome circuit breaker: an item that keeps failing the
//! same way while nothing that could change the outcome has changed gets no
//! further identical attempt. It waits on one investigation of the failure
//! instead.
//!
//! Three evidence kinds carry it, all written by the controller and none
//! verified (they are bookkeeping, never a result to hand on):
//!
//! - [`ATTEMPT_STATE`], at every lease: `hash` is a digest of what could
//!   plausibly change the outcome and `reference` the JSON it is a digest of
//!   ([`AttemptState`]): the item's objective, `done_when`, constraints and
//!   decisions (its answers included); its inputs with their statuses and
//!   verified commits; the workspace's base commit; the worker, its kind and
//!   a digest of its effective access (its advertised tools and resources,
//!   and the access fields of the spec it was added with); the tools,
//!   servers and resources the item requires; the repair generation (every
//!   done item it waits on, with when it closed); the daemon's build; and
//!   which ladder rung put the attempt in the queue. Never a run id, a path,
//!   a timestamp, a process or a secret.
//! - [`ATTEMPT_FAILURE`], when a run's failure is judged (`fail` and a
//!   stall): its run, fingerprint and class, whether it counts, and as its
//!   `hash` the attempt state it ran with. Budget failures do not count; an
//!   interrupted run, or one that waited for a worker, is never judged.
//! - [`CIRCUIT_OPEN`], when the breaker trips: the runs and the fingerprint.
//!
//! The breaker trips when the last `breaker_repeats` counted failures share
//! one fingerprint and one attempt state ([`tripped`]). The item is then not
//! retried, repaired or switched: it is queued behind the failure's fix item
//! through the ladder's own prerequisite path (`file_internal`), reusing an
//! open one, with an `improve` rung that says the breaker tripped. When no
//! investigation can be filed (the open one is itself waiting on its owner)
//! the item surfaces once instead. Anything that changes the attempt state
//! (a new repair generation, an answer, a changed input, an access repair, a
//! new build) starts the count again.
//!
//! The ladder's own rungs are bounded per item, so the rung that granted an
//! attempt is part of its state: two retries and two repairs are two
//! different approaches, while an attempt something else revived with
//! nothing changed (the 491-run loop of v6.5.6) is the same one again.

use chrono::Utc;
use rustykrab_core::questions::QuestionKind;
use rustykrab_core::work::{
    ErrorClass, Evidence, Rung, RungEvent, Status, WorkError, WorkEvent, WorkItem, WorkItemId,
};
use rustykrab_core::Error;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::graph::Snapshot;
use crate::investigate::RESOLUTION_WAIT;
use crate::ladder::SurfaceReason;
use crate::registry::WorkerSpec;
use crate::worker::{Brief, Worker};

use super::batch::Batch;
use super::brief::RUN;
use super::notice::label;
use super::questions::NewQuestion;
use super::tick::surface_reason;
use super::Controller;

/// The evidence kind of what an attempt ran with, written at its lease.
pub(super) const ATTEMPT_STATE: &str = "attempt_state";

/// The evidence kind of a judged failed attempt.
pub(super) const ATTEMPT_FAILURE: &str = "attempt_failure";

/// The evidence kind of a tripped breaker.
pub(super) const CIRCUIT_OPEN: &str = "circuit_open";

/// What a tripped breaker's rung outcome and transition say.
pub(super) const BREAKER_TRIPPED: &str = "circuit breaker tripped";

/// Hex characters kept of a digest, as many as a failure fingerprint has.
const DIGEST_LEN: usize = 16;

/// A stable digest of `value`'s JSON.
pub(super) fn digest(value: &impl Serialize) -> String {
    let json = serde_json::to_string(value).unwrap_or_default();
    let mut hex = hex::encode(Sha256::digest(json.as_bytes()));
    hex.truncate(DIGEST_LEN);
    hex
}

/// What could plausibly change an attempt's outcome. Field order is fixed
/// and every list is sorted where its order means nothing, so the same
/// state always serialises, and digests, the same.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct AttemptState {
    pub objective: String,
    pub done_when: String,
    pub constraints: Vec<String>,
    /// The item's decisions and the answers its brief carries, in order.
    pub decisions: Vec<String>,
    pub inputs: Vec<InputState>,
    /// Inputs past the brief's cap, by id.
    pub more_inputs: Vec<String>,
    /// The commit the run's worktree was cut from.
    pub base: Option<String>,
    pub worker: String,
    pub worker_kind: String,
    /// A digest of the worker's effective access ([`Controller::access_digest`]).
    pub access: String,
    pub required_tools: Vec<String>,
    pub required_mcp_servers: Vec<String>,
    pub writable_resources: Vec<String>,
    /// Every done item the attempt waits on, with when it closed.
    pub generation: Vec<Repair>,
    /// `<version>+<commit>` of the daemon.
    pub build: String,
    /// The ladder rung that queued the attempt: `retry`, `repair`,
    /// `switch_worker`, or `other` (its first attempt, an answer, a
    /// prerequisite that landed, any other revival).
    pub granted_by: String,
}

/// One input as the attempt saw it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct InputState {
    pub item: String,
    pub status: String,
    /// Its commits that git verified.
    pub commits: Vec<String>,
}

/// One done item the attempt waits on: a prerequisite, a capability or a
/// fix item, through the edge the item holds on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Repair {
    pub item: String,
    pub kind: String,
    pub edge: String,
    pub closed_at: Option<String>,
}

/// A judged failed attempt, as its [`ATTEMPT_FAILURE`] row records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct FailedAttempt {
    pub run: String,
    pub fingerprint: String,
    /// `<class>/<subclass>`.
    pub error: String,
    /// Whether it counts toward the breaker: a budget failure does not.
    pub counted: bool,
    /// The [`ATTEMPT_STATE`] digest it ran with; `None` for a run leased
    /// before attempt states were recorded, which never matches.
    #[serde(skip)]
    pub state: Option<String>,
}

/// A tripped breaker: the runs that failed the same way in the same state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Trip {
    pub runs: Vec<String>,
    pub fingerprint: String,
    pub state: String,
}

/// Whether the last `repeats` counted failures in `history` (oldest first)
/// share one fingerprint and one known attempt state. Zero never trips.
pub(super) fn tripped(history: &[FailedAttempt], repeats: u32) -> Option<Trip> {
    let n = usize::try_from(repeats).ok().filter(|n| *n > 0)?;
    let counted: Vec<&FailedAttempt> = history.iter().filter(|f| f.counted).collect();
    let last = counted.get(counted.len().checked_sub(n)?..)?;
    let head = last.last()?;
    let state = head.state.as_ref()?;
    last.iter()
        .all(|f| f.fingerprint == head.fingerprint && f.state.as_ref() == Some(state))
        .then(|| Trip {
            runs: last.iter().map(|f| f.run.clone()).collect(),
            fingerprint: head.fingerprint.clone(),
            state: state.clone(),
        })
}

/// Whether a failure counts toward the breaker. A spent budget is the
/// cumulative guard's to bound, not a repeated outcome.
fn counts(error: &WorkError) -> bool {
    error.class != ErrorClass::Budget
}

/// The rung that put the item back in the queue: its latest rung when that
/// is one of the ladder's own re-attempts, else `other`.
pub(super) fn granted_by(events: &[WorkEvent]) -> &'static str {
    match events
        .iter()
        .rev()
        .find_map(crate::ladder::rung_event)
        .map(|r| r.rung)
    {
        Some(Rung::Retry) => "retry",
        Some(Rung::Repair) => "repair",
        Some(Rung::SwitchWorker) => "switch_worker",
        _ => "other",
    }
}

/// The done items `item` waits on through an ordering edge, by id.
pub(super) fn generation(snap: &Snapshot, item: &str) -> Vec<Repair> {
    let mut out: Vec<Repair> = snap
        .edges_held_by(item)
        .filter(|e| e.kind.is_ordering())
        .filter_map(|e| {
            let up = snap.item(&e.depends_on)?;
            (up.status == Status::Done).then(|| Repair {
                item: up.id.clone(),
                kind: up.kind.as_str().to_string(),
                edge: e.kind.as_str().to_string(),
                closed_at: up.closed_at.map(|t| t.to_rfc3339()),
            })
        })
        .collect();
    out.sort_by(|a, b| a.item.cmp(&b.item).then_with(|| a.edge.cmp(&b.edge)));
    out.dedup();
    out
}

fn sorted(list: &[String]) -> Vec<String> {
    let mut out = list.to_vec();
    out.sort();
    out.dedup();
    out
}

/// The access fields of a stored worker spec: what changes what its agent
/// may do. Never its token (which is not stored), its environment's values
/// (only the names it passes through) or a login directory.
fn spec_access(spec: &WorkerSpec) -> serde_json::Value {
    serde_json::json!({
        "kind": spec.kind.as_str(),
        "repos": sorted(&spec.repos),
        "writable_files": sorted(&spec.writable_files),
        "readable_files": sorted(&spec.readable_files),
        "model": spec.model,
        "allowed_tools": sorted(&spec.allowed_tools),
        "denied_tools": sorted(&spec.denied_tools),
        "max_turns": spec.max_turns,
        "permission_mode": spec.permission_mode,
        "env_names": sorted(&spec.env),
        "require_max": spec.require_max,
        "require_chatgpt": spec.require_chatgpt,
        "base_url": spec.base_url,
    })
}

/// The run being judged and the attempt state it was leased with: the
/// latest `run` row, and the `attempt_state` row its lease wrote after it.
/// Both are written at lease time in one transaction, so their order is
/// their lease's.
fn current_attempt(evidence: &[Evidence]) -> Option<(String, Option<String>)> {
    let lease_rows: Vec<&Evidence> = evidence
        .iter()
        .filter(|e| e.kind == RUN || e.kind == ATTEMPT_STATE)
        .collect();
    let at = lease_rows.iter().rposition(|e| e.kind == RUN)?;
    let run = lease_rows[at].reference.clone();
    let state = lease_rows
        .get(at + 1)
        .filter(|e| e.kind == ATTEMPT_STATE)
        .and_then(|e| e.hash.clone());
    Some((run, state))
}

/// The failed attempts an item's evidence records, oldest first.
fn failed_attempts(evidence: &[Evidence]) -> Vec<FailedAttempt> {
    evidence
        .iter()
        .filter(|e| e.kind == ATTEMPT_FAILURE)
        .filter_map(|e| {
            let mut f: FailedAttempt = serde_json::from_str(&e.reference).ok()?;
            f.state = e.hash.clone();
            Some(f)
        })
        .collect()
}

impl Controller {
    /// A digest of what `worker`'s runs may do: what it advertises
    /// ([`Worker::access_profile`]) and, for a worker added with a spec, the
    /// spec's access fields ([`spec_access`]).
    pub(super) async fn access_digest(&self, worker: &dyn Worker) -> String {
        let spec = match self.registry.store().workers().get(worker.name()).await {
            Ok(row) => row
                .and_then(|r| serde_json::from_value::<WorkerSpec>(r.config).ok())
                .filter(|s| s != &WorkerSpec::default())
                .map(|s| spec_access(&s)),
            Err(e) => {
                tracing::debug!(worker = %worker.name(), error = %e, "worker spec not read");
                None
            }
        };
        digest(&serde_json::json!({
            "advertised": worker.access_profile(),
            "spec": spec,
        }))
    }

    /// What this attempt runs with, as its [`ATTEMPT_STATE`] row: the brief
    /// as built for `worker`, the snapshot it was built over and the
    /// item's events before the lease.
    pub(super) async fn attempt_state(
        &self,
        snap: &Snapshot,
        item: &WorkItem,
        brief: &Brief,
        worker: &dyn Worker,
        events: &[WorkEvent],
    ) -> Result<Evidence, Error> {
        let mut inputs: Vec<InputState> = brief
            .inputs
            .iter()
            .map(|i| InputState {
                item: i.item.clone(),
                status: i.status.to_string(),
                commits: sorted(
                    &i.evidence
                        .iter()
                        .filter(|r| r.kind == "commit")
                        .map(|r| r.value.clone())
                        .collect::<Vec<_>>(),
                ),
            })
            .collect();
        inputs.sort_by(|a, b| a.item.cmp(&b.item));
        let state = AttemptState {
            objective: brief.objective.clone(),
            done_when: brief.done_when.clone(),
            constraints: brief.constraints.clone(),
            decisions: brief.decisions_made.clone(),
            inputs,
            more_inputs: sorted(&brief.more_inputs),
            base: brief.workspace.as_ref().map(|w| w.base.clone()),
            worker: worker.name().to_string(),
            worker_kind: worker.kind().as_str().to_string(),
            access: self.access_digest(worker).await,
            required_tools: sorted(&brief.required_tools),
            required_mcp_servers: sorted(&brief.required_mcp_servers),
            writable_resources: sorted(&brief.writable_resources),
            generation: generation(snap, &item.id),
            build: self.build_label(),
            granted_by: granted_by(events).to_string(),
        };
        Ok(Evidence {
            item: item.id.clone(),
            kind: ATTEMPT_STATE.to_string(),
            reference: serde_json::to_string(&state)
                .map_err(|e| Error::Internal(format!("attempt state: {e}")))?,
            hash: Some(digest(&state)),
            verified_by: None,
            at: Utc::now(),
        })
    }

    /// Record the failure of the run being judged as an [`ATTEMPT_FAILURE`]
    /// row and say whether the breaker trips on it. Nothing to record when
    /// the item has no run on record.
    pub(super) async fn judge_attempt(
        &self,
        b: &mut Batch,
        item: &WorkItem,
        error: &WorkError,
    ) -> Result<Option<Trip>, Error> {
        let evidence = self.store.work_evidence_list(&item.id).await?;
        let Some((run, state)) = current_attempt(&evidence) else {
            return Ok(None);
        };
        let failure = FailedAttempt {
            run,
            fingerprint: error.fingerprint.clone(),
            error: format!("{}/{}", error.class.as_str(), error.subclass.as_str()),
            counted: counts(error),
            state,
        };
        b.evidence.push(Evidence {
            item: item.id.clone(),
            kind: ATTEMPT_FAILURE.to_string(),
            reference: serde_json::to_string(&failure)
                .map_err(|e| Error::Internal(format!("attempt failure: {e}")))?,
            hash: failure.state.clone(),
            verified_by: None,
            at: b.now,
        });
        // Each judged run is one entry. Not keyed by run id: a scheduled
        // firing runs every attempt under its job's one conversation id.
        let mut history = failed_attempts(&evidence);
        history.push(failure);
        Ok(tripped(&history, self.config.breaker_repeats))
    }

    /// Carry out a tripped breaker: a [`CIRCUIT_OPEN`] row, then the item
    /// queued behind the failure's one investigation (created or reused by
    /// `file_internal`) with an `improve` rung that says the breaker
    /// tripped; or, when no investigation can be filed, the item surfaced
    /// once with a question. Returns the ids to settle.
    pub(super) async fn trip(
        &self,
        b: &mut Batch,
        item: &WorkItem,
        error: &WorkError,
        trip: Trip,
    ) -> Result<Vec<WorkItemId>, Error> {
        let mut ladder = self.ladder_of(item).await?;
        let n = trip.runs.len();
        let what = format!(
            "{BREAKER_TRIPPED}: {n} failed attempts in a row with failure {} ({}/{}) and \
             unchanged attempt state {} (runs {})",
            trip.fingerprint,
            error.class.as_str(),
            error.subclass.as_str(),
            trip.state,
            trip.runs.join(", ")
        );
        b.evidence.push(Evidence {
            item: item.id.clone(),
            kind: CIRCUIT_OPEN.to_string(),
            reference: serde_json::json!({
                "fingerprint": trip.fingerprint,
                "error": format!("{}/{}: {}", error.class.as_str(), error.subclass.as_str(), error.detail),
                "attempt_state": trip.state,
                "repeats": n,
                "runs": trip.runs,
            })
            .to_string(),
            hash: Some(trip.fingerprint.clone()),
            verified_by: None,
            at: b.now,
        });
        tracing::warn!(item = %item.id, fingerprint = %trip.fingerprint, runs = ?trip.runs, "{BREAKER_TRIPPED}");
        let mut changed = b.move_to(
            &item.id,
            Status::Queued,
            "controller",
            format!("{what}; waiting for an investigation"),
        );
        let source = self.source_checkout(&[]).await;
        match self.file_internal(b, item, error, source.as_ref()) {
            Ok(filed) => {
                b.rung(
                    &item.id,
                    &mut ladder,
                    RungEvent {
                        rung: Rung::Improve,
                        at: b.now,
                        error: Some(error.clone()),
                        outcome: format!("{RESOLUTION_WAIT}{filed}; {what}"),
                    },
                );
                changed.push(filed);
            }
            Err(why) => {
                let reason = surface_reason(error, SurfaceReason::LadderSpent);
                let ask = format!(
                    "{} failed {n} times in a row the same way ({}/{}: {}) with nothing changed \
                     between attempts, and no investigation could be filed: {why}. How should it \
                     go on, or should it stop?",
                    label(item),
                    error.class.as_str(),
                    error.subclass.as_str(),
                    error.detail
                );
                b.rung(
                    &item.id,
                    &mut ladder,
                    RungEvent {
                        rung: Rung::Surface,
                        at: b.now,
                        error: Some(error.clone()),
                        outcome: format!("{what}; investigation not filed: {why}"),
                    },
                );
                changed.extend(b.move_to(
                    &item.id,
                    Status::Blocked(reason),
                    "controller",
                    ask.clone(),
                ));
                self.record_question(
                    b,
                    NewQuestion::open(
                        &item.id,
                        QuestionKind::for_blocked(reason),
                        ask,
                        "circuit_breaker",
                    ),
                )
                .await?;
            }
        }
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed(run: &str, fp: &str, state: Option<&str>, counted: bool) -> FailedAttempt {
        FailedAttempt {
            run: run.into(),
            fingerprint: fp.into(),
            error: "tool/invalid_args".into(),
            counted,
            state: state.map(str::to_string),
        }
    }

    #[test]
    fn three_identical_failures_trip_and_name_their_runs() {
        let history = [
            failed("r1", "fp", Some("s"), true),
            failed("r2", "fp", Some("s"), true),
            failed("r3", "fp", Some("s"), true),
        ];
        assert_eq!(tripped(&history[..2], 3), None);
        assert_eq!(
            tripped(&history, 3),
            Some(Trip {
                runs: vec!["r1".into(), "r2".into(), "r3".into()],
                fingerprint: "fp".into(),
                state: "s".into(),
            })
        );
    }

    #[test]
    fn a_changed_state_or_fingerprint_starts_the_count_again() {
        let changed_state = [
            failed("r1", "fp", Some("s"), true),
            failed("r2", "fp", Some("t"), true),
            failed("r3", "fp", Some("t"), true),
        ];
        assert_eq!(tripped(&changed_state, 3), None);
        let changed_failure = [
            failed("r1", "other", Some("s"), true),
            failed("r2", "fp", Some("s"), true),
            failed("r3", "fp", Some("s"), true),
        ];
        assert_eq!(tripped(&changed_failure, 3), None);
        // A state nobody recorded (a lease before this build) never matches.
        let unknown = [
            failed("r1", "fp", None, true),
            failed("r2", "fp", None, true),
            failed("r3", "fp", None, true),
        ];
        assert_eq!(tripped(&unknown, 3), None);
    }

    #[test]
    fn budget_failures_do_not_count_or_break_the_run() {
        let history = [
            failed("r1", "fp", Some("s"), true),
            failed("r2", "budget", Some("s"), false),
            failed("r3", "fp", Some("s"), true),
            failed("r4", "fp", Some("s"), true),
        ];
        let trip = tripped(&history, 3).expect("three counted failures trip");
        assert_eq!(trip.runs, vec!["r1", "r3", "r4"]);
    }

    #[test]
    fn zero_repeats_turns_the_breaker_off() {
        let history = [failed("r1", "fp", Some("s"), true)];
        assert_eq!(tripped(&history, 0), None);
        assert!(tripped(&history, 1).is_some());
    }

    #[test]
    fn the_rung_that_queued_an_attempt_is_part_of_its_state() {
        let rung = |rung: Rung| WorkEvent {
            item: "i".into(),
            at: Utc::now(),
            kind: rustykrab_core::work::EventKind::Rung,
            from: None,
            to: None,
            actor: "controller".into(),
            reason: Some(crate::ladder::encode_rung_event(&RungEvent {
                rung,
                at: Utc::now(),
                error: None,
                outcome: String::new(),
            })),
            upstream: None,
            origin: None,
            evidence_ref: None,
        };
        assert_eq!(granted_by(&[]), "other");
        assert_eq!(granted_by(&[rung(Rung::Retry)]), "retry");
        assert_eq!(
            granted_by(&[rung(Rung::Retry), rung(Rung::Repair)]),
            "repair"
        );
        assert_eq!(
            granted_by(&[rung(Rung::Repair), rung(Rung::Surface)]),
            "other"
        );
        assert_eq!(granted_by(&[rung(Rung::Improve)]), "other");
    }

    #[test]
    fn digests_are_stable_and_short() {
        let a = digest(&serde_json::json!({"a": 1}));
        assert_eq!(a, digest(&serde_json::json!({"a": 1})));
        assert_ne!(a, digest(&serde_json::json!({"a": 2})));
        assert_eq!(a.len(), DIGEST_LEN);
    }
}

#[cfg(test)]
mod controller_tests {
    use super::*;
    use crate::controller::tests::{draft, failure, report, Harness};
    use crate::controller::{ControllerConfig, StaticCatalog};
    use rustykrab_core::work::{Budget, ErrorSubclass, RungBudgets};

    fn spec() -> WorkerSpec {
        WorkerSpec {
            kind: rustykrab_core::work::WorkerKind::ClaudeCode,
            repos: vec!["/src/app".into()],
            allowed_tools: vec!["Bash(cargo:*)".into()],
            permission_mode: Some("acceptEdits".into()),
            env: vec!["GH_TOKEN".into()],
            ..WorkerSpec::default()
        }
    }

    #[test]
    fn an_access_repair_changes_the_identity_and_nothing_else_does() {
        let before = digest(&spec_access(&spec()));
        // What the agent may do: each changes it.
        let mut granted = spec();
        granted.allowed_tools.push("Read(/data/**)".into());
        let mut denied = spec();
        denied.denied_tools.push("Read(~/.ssh/**)".into());
        let mut mode = spec();
        mode.permission_mode = Some("bypassPermissions".into());
        let mut file = spec();
        file.readable_files.push("/data/input.csv".into());
        for changed in [granted, denied, mode, file] {
            assert_ne!(digest(&spec_access(&changed)), before, "{changed:?}");
        }
        // How it runs, not what it may do: none changes it.
        let mut tuned = spec();
        tuned.timeout_seconds = Some(60);
        tuned.concurrency = Some(4);
        tuned.command = Some("/opt/claude".into());
        tuned.claude_config_dir = Some("/Users/me/.claude-work".into());
        tuned.token = Some("never-stored".into());
        tuned.allowed_tools.reverse();
        assert_eq!(digest(&spec_access(&tuned)), before);
        // Environment variables by name only: no value is read.
        let text = spec_access(&spec()).to_string();
        assert!(text.contains("GH_TOKEN") && !text.contains("never-stored"));
    }

    #[tokio::test]
    async fn zero_repeats_turns_the_breaker_off() {
        let h = Harness::with(
            ControllerConfig {
                breaker_repeats: 0,
                ..ControllerConfig::default()
            },
            StaticCatalog::default(),
            &["pinch"],
        );
        for _ in 0..5 {
            h.script.push(
                "Read the tide gauge",
                report(failure(
                    ErrorSubclass::InvalidArgs,
                    "the gauge rejected station 7",
                )),
            );
        }
        let mut d = draft("x", "Read the tide gauge");
        d.budget = Some(Budget {
            iterations: 60,
            tokens: 100_000,
            wall_seconds: 3_600,
            repairs: 10,
            rungs: RungBudgets {
                repairs: 10,
                ..RungBudgets::default()
            },
        });
        let x = h.file_one(d).await;
        h.drain().await;
        assert_eq!(h.status(&x).await, Status::Done);
        assert_eq!(h.script.briefs_for("Read the tide gauge").len(), 6);
        assert!(!h.rungs(&x).await.contains(&Rung::Improve));
    }
}
