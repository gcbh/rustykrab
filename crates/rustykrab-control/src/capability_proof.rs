//! Functional capability proof over host-owned observations.
//!
//! Catalog presence proves that instructions/tools are available, not that an
//! operation works. The controller supplies expected bindings from its own
//! state; an adapter records what it actually observed. Neither a model's
//! artifact nor its choice of expected bindings can establish this proof.
//!
//! This module performs no operation and changes no work state or authority.
//! Producers must strip model-authored `operation_probe` artifacts before
//! forwarding reports and write these evidence rows through a trusted path.
use chrono::{DateTime, Utc};
use rustykrab_core::work::{Evidence, WorkItem};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const OPERATION_PROBE: &str = "operation_probe";
const MAX_REFERENCE_BYTES: usize = 16_384;
const MAX_OBSERVATION_BYTES: usize = 4_096;

/// Expected bindings, selected by the controller rather than the asking model.
/// `inputs_digest` is canonical lowercase SHA-256 over inputs and effective
/// access/configuration, excluding secrets and incidental invocation paths.
/// `run` identifies the expected host probe invocation, not arbitrary CLI text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityRef {
    pub operation: String,
    pub executor: String,
    pub inputs_digest: String,
    pub generation: String,
    pub run: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeOutcome {
    Ok,
    Failed,
    Unobserved,
}

/// JSON in a host-written Evidence.reference. This is data, not authority;
/// deserializing it does not construct a Proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationProbe {
    pub operation: String,
    pub executor: String,
    pub inputs_digest: String,
    pub generation: String,
    pub run: String,
    pub outcome: ProbeOutcome,
    /// A bounded, credential-free reference to the observed result/trace.
    pub observation: String,
}

/// Only the validator constructs this type. It attests this observation and its
/// bindings, not the completion of the owner goal or permission to resume work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proof {
    probe: OperationProbe,
    observed_at: DateTime<Utc>,
}

impl Proof {
    pub fn probe(&self) -> &OperationProbe {
        &self.probe
    }

    pub fn observed_at(&self) -> DateTime<Utc> {
        self.observed_at
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum Missing {
    #[error("invalid expected probe binding: {0}")]
    InvalidNeed(&'static str),
    #[error("no operation probe is recorded")]
    NoProbe,
    #[error("operation probe belongs to another work item")]
    ForeignItem,
    #[error("latest probe records contradict each other")]
    AmbiguousProbe,
    #[error("operation probe lacks trusted executor provenance")]
    Untrusted,
    #[error("operation probe is malformed, oversized or incomplete")]
    MalformedProbe,
    #[error("probe exercised another operation")]
    WrongOperation,
    #[error("probe used another executor")]
    WrongExecutor,
    #[error("probe inputs or effective access do not match")]
    WrongInputs,
    #[error("probe is from another repair/configuration generation")]
    StaleGeneration,
    #[error("probe is from another invocation")]
    WrongRun,
    #[error("the operation probe failed")]
    Failed,
    #[error("the operation was not observed")]
    Unobserved,
}

fn identity(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Validate the latest observation for this item. A later failure or incomplete
/// record cannot be hidden by an earlier success, regardless of storage order.
/// Exact duplicate latest rows are idempotent; contradictory tied rows fail.
///
/// The caller must load evidence from host-owned storage and enforce the
/// independent consent, dependency, availability and #693 freshness gates.
/// This function cannot authenticate an arbitrary caller-provided Evidence.
pub fn capability_proven(
    item: &WorkItem,
    evidence: &[Evidence],
    need: &CapabilityRef,
) -> Result<Proof, Missing> {
    for (field, valid) in [
        ("operation", identity(&need.operation, 1_024)),
        ("executor", identity(&need.executor, 128)),
        ("inputs_digest", digest(&need.inputs_digest)),
        ("generation", identity(&need.generation, 512)),
        ("run", identity(&need.run, 128)),
    ] {
        if !valid {
            return Err(Missing::InvalidNeed(field));
        }
    }
    let latest = evidence
        .iter()
        .filter(|e| e.kind == OPERATION_PROBE && e.item == item.id)
        .max_by_key(|e| e.at)
        .ok_or_else(|| {
            if evidence.iter().any(|e| e.kind == OPERATION_PROBE) {
                Missing::ForeignItem
            } else {
                Missing::NoProbe
            }
        })?;
    if evidence
        .iter()
        .any(|e| e.kind == OPERATION_PROBE && e.item == item.id && e.at == latest.at && e != latest)
    {
        return Err(Missing::AmbiguousProbe);
    }
    let verifier = format!("executor:{}", need.executor);
    if latest.verified_by.as_deref() != Some(verifier.as_str()) {
        return Err(Missing::Untrusted);
    }
    if latest.reference.len() > MAX_REFERENCE_BYTES || latest.at < item.created_at {
        return Err(Missing::MalformedProbe);
    }
    let probe: OperationProbe =
        serde_json::from_str(&latest.reference).map_err(|_| Missing::MalformedProbe)?;
    if probe.observation.trim().is_empty() || probe.observation.len() > MAX_OBSERVATION_BYTES {
        return Err(Missing::MalformedProbe);
    }
    if probe.operation != need.operation {
        return Err(Missing::WrongOperation);
    }
    if probe.executor != need.executor {
        return Err(Missing::WrongExecutor);
    }
    if probe.inputs_digest != need.inputs_digest
        || latest.hash.as_deref() != Some(need.inputs_digest.as_str())
    {
        return Err(Missing::WrongInputs);
    }
    if probe.generation != need.generation {
        return Err(Missing::StaleGeneration);
    }
    if probe.run != need.run {
        return Err(Missing::WrongRun);
    }
    match probe.outcome {
        ProbeOutcome::Failed => Err(Missing::Failed),
        ProbeOutcome::Unobserved => Err(Missing::Unobserved),
        ProbeOutcome::Ok => Ok(Proof {
            probe,
            observed_at: latest.at,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustykrab_core::work::{BlockedReason, Budget, Status, Trigger, WorkKind};

    fn fixture() -> (WorkItem, CapabilityRef, Evidence) {
        let at = DateTime::parse_from_rfc3339("2026-10-14T20:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let item = WorkItem {
            id: "capability-fixture".into(),
            kind: WorkKind::Capability,
            title: "Verify repository access".into(),
            objective: "Read on assigned executor".into(),
            done_when: "The required read succeeds".into(),
            constraints: vec![],
            decisions_made: vec![],
            artifact_refs: vec![],
            required_tools: vec![],
            required_mcp_servers: vec![],
            worker_kind: Default::default(),
            writable_resources: vec![],
            parent: None,
            inputs_from: vec![],
            origin_conversation_id: None,
            trigger: Trigger::Now,
            preconditions: vec![],
            expires_at: None,
            budget: Budget::default(),
            priority: 0,
            status: Status::Queued,
            status_origin: None,
            plan_id: None,
            held_by: None,
            created_at: at,
            updated_at: at,
            closed_at: None,
        };
        let need = CapabilityRef {
            operation: "repository.read".into(),
            executor: "max-fieldkit".into(),
            inputs_digest: "a".repeat(64),
            generation: "repair-2/access-4/build-619d476".into(),
            run: "probe-2".into(),
        };
        let probe = OperationProbe {
            operation: need.operation.clone(),
            executor: need.executor.clone(),
            inputs_digest: need.inputs_digest.clone(),
            generation: need.generation.clone(),
            run: need.run.clone(),
            outcome: ProbeOutcome::Ok,
            observation: "host-trace:probe-2/read-1; exit=0; expected bytes matched".into(),
        };
        let evidence = Evidence {
            item: item.id.clone(),
            kind: OPERATION_PROBE.into(),
            reference: serde_json::to_string(&probe).unwrap(),
            hash: Some(need.inputs_digest.clone()),
            verified_by: Some(format!("executor:{}", need.executor)),
            at,
        };
        (item, need, evidence)
    }

    fn change(evidence: &Evidence, f: impl FnOnce(&mut OperationProbe)) -> Evidence {
        let mut probe = serde_json::from_str(&evidence.reference).unwrap();
        f(&mut probe);
        Evidence {
            reference: serde_json::to_string(&probe).unwrap(),
            ..evidence.clone()
        }
    }

    #[test]
    fn exact_host_observation_proves_operation_without_changing_owner_holds() {
        let (mut item, need, evidence) = fixture();
        item.status = Status::Blocked(BlockedReason::NeedsConsent);
        item.held_by = Some("independent-owner-question".into());
        let before = item.clone();
        let proof = capability_proven(&item, std::slice::from_ref(&evidence), &need).unwrap();
        assert_eq!(proof.probe().run, need.run);
        assert_eq!(proof.observed_at(), evidence.at);
        assert_eq!(
            item, before,
            "operation proof does not grant authority or resume work"
        );
    }

    #[test]
    fn skill_catalog_record_and_model_claim_are_not_functional_proof() {
        let (item, need, evidence) = fixture();
        let catalog = Evidence {
            kind: "tool".into(),
            reference: need.operation.clone(),
            verified_by: Some("catalog".into()),
            ..evidence.clone()
        };
        assert_eq!(
            capability_proven(&item, &[catalog], &need),
            Err(Missing::NoProbe)
        );
        let model = Evidence {
            verified_by: None,
            ..evidence.clone()
        };
        assert_eq!(
            capability_proven(&item, &[model], &need),
            Err(Missing::Untrusted)
        );
        let another = Evidence {
            verified_by: Some("executor:codex-primary".into()),
            ..evidence
        };
        assert_eq!(
            capability_proven(&item, &[another], &need),
            Err(Missing::Untrusted)
        );
    }

    #[test]
    fn every_binding_blocks_replay_on_another_operation_or_execution() {
        let (item, need, evidence) = fixture();
        let cases = [
            (
                change(&evidence, |p| p.operation = "controller.dispatch".into()),
                Missing::WrongOperation,
            ),
            (
                change(&evidence, |p| p.executor = "planner".into()),
                Missing::WrongExecutor,
            ),
            (
                change(&evidence, |p| p.inputs_digest = "b".repeat(64)),
                Missing::WrongInputs,
            ),
            (
                change(&evidence, |p| p.generation = "repair-1".into()),
                Missing::StaleGeneration,
            ),
            (
                change(&evidence, |p| p.run = "earlier-run".into()),
                Missing::WrongRun,
            ),
            (
                Evidence {
                    hash: Some("b".repeat(64)),
                    ..evidence.clone()
                },
                Missing::WrongInputs,
            ),
            (
                Evidence {
                    item: "another-item".into(),
                    ..evidence.clone()
                },
                Missing::ForeignItem,
            ),
        ];
        for (row, missing) in cases {
            assert_eq!(capability_proven(&item, &[row], &need), Err(missing));
        }
    }

    #[test]
    fn later_failed_or_unobserved_probe_cannot_be_hidden_by_earlier_success() {
        let (item, need, success) = fixture();
        for (outcome, missing) in [
            (ProbeOutcome::Failed, Missing::Failed),
            (ProbeOutcome::Unobserved, Missing::Unobserved),
        ] {
            let mut later = change(&success, |p| p.outcome = outcome);
            later.at += chrono::Duration::seconds(1);
            for rows in [
                vec![success.clone(), later.clone()],
                vec![later.clone(), success.clone()],
            ] {
                assert_eq!(capability_proven(&item, &rows, &need), Err(missing.clone()));
            }
        }
    }

    #[test]
    fn tied_conflicting_probes_fail_but_identical_duplicates_are_idempotent() {
        let (item, need, evidence) = fixture();
        let failed = change(&evidence, |p| p.outcome = ProbeOutcome::Failed);
        for rows in [
            vec![evidence.clone(), failed.clone()],
            vec![failed, evidence.clone()],
        ] {
            assert_eq!(
                capability_proven(&item, &rows, &need),
                Err(Missing::AmbiguousProbe)
            );
        }
        assert!(capability_proven(&item, &[evidence.clone(), evidence], &need).is_ok());
    }

    #[test]
    fn malformed_or_incomplete_latest_observation_is_visible_instead_of_old_success() {
        let (item, need, evidence) = fixture();
        let mut unknown = serde_json::from_str::<serde_json::Value>(&evidence.reference).unwrap();
        unknown["model_claimed_verified_by"] = "executor:max-fieldkit".into();
        let mut no_run = serde_json::from_str::<serde_json::Value>(&evidence.reference).unwrap();
        no_run.as_object_mut().unwrap().remove("run");
        let cases = [
            Evidence {
                reference: "{broken".into(),
                ..evidence.clone()
            },
            Evidence {
                reference: serde_json::to_string(&unknown).unwrap(),
                ..evidence.clone()
            },
            Evidence {
                reference: serde_json::to_string(&no_run).unwrap(),
                ..evidence.clone()
            },
            Evidence {
                reference: "x".repeat(MAX_REFERENCE_BYTES + 1),
                ..evidence.clone()
            },
            change(&evidence, |p| p.observation.clear()),
            change(&evidence, |p| {
                p.observation = "x".repeat(MAX_OBSERVATION_BYTES + 1)
            }),
        ];
        for mut bad in cases {
            bad.at += chrono::Duration::seconds(1);
            assert_eq!(
                capability_proven(&item, &[evidence.clone(), bad], &need),
                Err(Missing::MalformedProbe)
            );
        }
    }

    #[test]
    fn incomplete_expected_bindings_cannot_match_incomplete_observations() {
        let (item, need, evidence) = fixture();
        let mut empty = need.clone();
        empty.generation.clear();
        assert_eq!(
            capability_proven(&item, std::slice::from_ref(&evidence), &empty),
            Err(Missing::InvalidNeed("generation"))
        );
        let mut invalid = need;
        invalid.inputs_digest = "opaque-model-label".into();
        assert_eq!(
            capability_proven(&item, &[evidence], &invalid),
            Err(Missing::InvalidNeed("inputs_digest"))
        );
    }

    #[tokio::test]
    async fn durable_evidence_preserves_bindings_and_later_failure_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let (item, need, success) = fixture();
        let store = rustykrab_store::Store::open(dir.path(), vec![4; 32]).unwrap();
        store
            .work_insert_graph(std::slice::from_ref(&item), &[], None)
            .await
            .unwrap();
        store.work_evidence_add(success.clone()).await.unwrap();
        drop(store);

        let store = rustykrab_store::Store::open(dir.path(), vec![4; 32]).unwrap();
        let loaded = store.work_get(&item.id).await.unwrap().unwrap();
        let rows = store.work_evidence_list(&item.id).await.unwrap();
        assert!(capability_proven(&loaded, &rows, &need).is_ok());
        let mut failed = change(&success, |p| p.outcome = ProbeOutcome::Failed);
        failed.at += chrono::Duration::seconds(1);
        store.work_evidence_add(failed).await.unwrap();
        drop(store);

        let store = rustykrab_store::Store::open(dir.path(), vec![4; 32]).unwrap();
        let rows = store.work_evidence_list(&item.id).await.unwrap();
        assert_eq!(
            capability_proven(&loaded, &rows, &need),
            Err(Missing::Failed)
        );
        assert_eq!(
            store.work_get(&item.id).await.unwrap().unwrap().status,
            Status::Queued
        );
    }
}
