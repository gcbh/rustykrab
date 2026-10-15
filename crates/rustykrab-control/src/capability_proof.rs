//! Functional capability proof over host-owned observations.
//!
//! Catalog presence proves that instructions/tools are available, not that an
//! operation works. The controller supplies expected bindings from its own
//! state; an adapter records what it actually observed. Neither a model's
//! artifact nor its choice of expected bindings can establish this proof.
//!
//! Validation changes no work state or authority. The trusted wrapper relay
//! delegates a fixed observation to Worker; concrete execution stays in adapters.
//! Producers must strip model-authored `operation_probe` artifacts before
//! forwarding reports and write these evidence rows through a trusted path.
use chrono::{DateTime, Utc};
use rustykrab_core::{
    types::ToolCall,
    work::{Evidence, WorkItem},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{path::PathBuf, time::Duration};
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
pub struct ExpectedOperation {
    pub operation: String,
    pub executor: String,
    pub inputs_digest: String,
    pub generation: String,
    pub run: String,
}

impl ExpectedOperation {
    /// Observation data alone does not construct a Proof.
    pub fn observation(
        &self,
        outcome: ProbeOutcome,
        observation: impl Into<String>,
    ) -> OperationProbe {
        OperationProbe {
            operation: self.operation.clone(),
            executor: self.executor.clone(),
            inputs_digest: self.inputs_digest.clone(),
            generation: self.generation.clone(),
            run: self.run.clone(),
            outcome,
            observation: observation.into(),
        }
    }
}

/// Host-selected, effect-free availability checks. These do not attest a build,
/// test, repository read or the completion of the original owner's goal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RegisteredAction {
    BinaryAvailable {
        binary: String,
    },
    ToolCallable {
        tool: String,
        args: serde_json::Value,
    },
}

/// Shared need-to-operation registry for controller selection and adapter
/// invocation. Unsupported needs remain availability-only; arbitrary action
/// strings and arguments cannot expand this registry.
pub fn registered_action(need: &crate::routing::CapabilityRef) -> Option<RegisteredAction> {
    use crate::errors::GapKind;
    match (need.gap, need.subject.as_str()) {
        (GapKind::Tool | GapKind::Install, binary @ ("cargo" | "node")) => {
            Some(RegisteredAction::BinaryAvailable {
                binary: binary.into(),
            })
        }
        (GapKind::Tool, "work_resources") => Some(RegisteredAction::ToolCallable {
            tool: need.subject.clone(),
            args: serde_json::json!({}),
        }),
        _ => None,
    }
}

impl RegisteredAction {
    /// Fixed invocation data for a registered action. This does not execute a
    /// tool or grant access: the adapter still enforces its real tool boundary.
    /// Canonical equality rejects changed args, shell text and mismatched action
    /// variants through the same registry used by controller need selection.
    pub fn tool_call(&self, run: &str) -> Option<ToolCall> {
        use crate::{errors::GapKind, routing::CapabilityRef};
        let (gap, subject) = match self {
            Self::BinaryAvailable { binary } => (GapKind::Install, binary),
            Self::ToolCallable { tool, .. } => (GapKind::Tool, tool),
        };
        let canonical = registered_action(&CapabilityRef {
            gap,
            subject: subject.clone(),
        })?;
        if canonical != *self {
            return None;
        }
        let (name, arguments) = match canonical {
            Self::BinaryAvailable { binary } => (
                "exec".into(),
                serde_json::json!({
                    "command": format!("{binary} --version"), "timeout_secs": 30
                }),
            ),
            Self::ToolCallable { tool, args } => (tool, args),
        };
        Some(ToolCall {
            id: run.into(),
            name,
            arguments,
        })
    }

    pub fn operation(&self) -> String {
        match self {
            Self::BinaryAvailable { binary } => format!("binary.available/{binary}"),
            Self::ToolCallable { tool, .. } => format!("tool.callable/{tool}"),
        }
    }
}

/// Stable repository identity. A fresh worktree path is deliberately excluded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceIdentity {
    pub repo: PathBuf,
    pub base: String,
}

/// The execution-relevant portion of the real brief, never prompt text or grants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveRequirements {
    pub kind: rustykrab_core::work::WorkKind,
    pub required_tools: Vec<String>,
    pub required_mcp_servers: Vec<String>,
    pub writable_resources: Vec<String>,
    pub read_only_review: bool,
    pub workspace: Option<WorkspaceIdentity>,
}

impl EffectiveRequirements {
    pub fn from_item(item: &rustykrab_core::work::WorkItem) -> Self {
        Self {
            kind: item.kind,
            required_tools: item.required_tools.clone(),
            required_mcp_servers: item.required_mcp_servers.clone(),
            writable_resources: item.writable_resources.clone(),
            read_only_review: item
                .artifact_refs
                .iter()
                .any(|r| r.kind == rustykrab_core::dream_review::REVIEW_ONLY && r.value == "true"),
            workspace: None,
        }
    }

    pub fn from_brief(brief: &crate::worker::Brief) -> Self {
        Self {
            kind: brief.kind,
            required_tools: brief.required_tools.clone(),
            required_mcp_servers: brief.required_mcp_servers.clone(),
            writable_resources: brief.writable_resources.clone(),
            read_only_review: brief
                .artifact_refs
                .iter()
                .any(|r| r.kind == rustykrab_core::dream_review::REVIEW_ONLY && r.value == "true"),
            workspace: brief.workspace.as_ref().map(|w| WorkspaceIdentity {
                repo: w.repo.clone(),
                base: w.base.clone(),
            }),
        }
    }

    /// The actual subscription wrapper's host/native predicate, shared by run
    /// and probe selection. It confers no eligibility or permission.
    pub fn uses_host(&self) -> bool {
        !self.required_tools.is_empty()
            || self
                .writable_resources
                .iter()
                .any(|r| !r.starts_with("repo:"))
    }

    pub fn digest(&self) -> String {
        // Encode the path losslessly; serialization of a non-UTF8 PathBuf must
        // not panic or alias a different repository identity.
        binding_digest(&(
            self.kind,
            sorted(&self.required_tools),
            sorted(&self.required_mcp_servers),
            sorted(&self.writable_resources),
            self.read_only_review,
            self.workspace
                .as_ref()
                .map(|w| (w.repo.as_os_str().as_encoded_bytes(), &w.base)),
        ))
    }
}

/// Controller-owned execution context. Only this request carries a transient dir.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeContext {
    pub requirements: EffectiveRequirements,
    pub bound_dir: Option<PathBuf>,
}

impl ProbeContext {
    pub fn from_brief(brief: &crate::worker::Brief) -> Self {
        Self {
            requirements: EffectiveRequirements::from_brief(brief),
            bound_dir: brief.workspace.as_ref().map(|w| w.path.clone()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionPath {
    Local,
    Host,
    Native,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeScope {
    pub worker: String,
    pub path: ExecutionPath,
    pub policy_digest: String,
    pub ceiling_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeRequest {
    pub expected: ExpectedOperation,
    pub action: RegisteredAction,
    pub scope: ProbeScope,
    pub context: ProbeContext,
    pub deadline: Duration,
}

pub fn sorted(names: &[String]) -> Vec<String> {
    let mut names = names.to_vec();
    names.sort();
    names.dedup();
    names
}

/// Controlled, credential-free structural data only. Callers normalize set fields;
/// ordered arrays such as command arguments must retain their order.
pub fn binding_digest(value: &impl Serialize) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(value).expect("serializable binding"))
    )
}

/// Stable across registry order and duplicate names; contains no secret values.
pub fn tool_ceiling_digest(names: &[String]) -> String {
    binding_digest(&sorted(names))
}

/// Availability proof binds current requirements/path/policy, excluding incidental
/// run/worktree/deadline values. It still does not attest repository contents.
pub fn probe_inputs_digest(
    action: &RegisteredAction,
    scope: &ProbeScope,
    context: &ProbeContext,
) -> String {
    binding_digest(&(action, scope, context.requirements.digest()))
}

/// Trusted wrapper relay: validate its current outer boundary, ask the child
/// through the child's own boundary, then re-attest only a matching observation.
/// A model report cannot enter this path. No authority or action is expanded.
pub async fn probe_through(
    parent: &dyn crate::worker::Worker,
    child: &dyn crate::worker::Worker,
    request: &ProbeRequest,
) -> OperationProbe {
    let unobserved = |why| request.expected.observation(ProbeOutcome::Unobserved, why);
    if !request.valid() || parent.probe_scope(&request.context).as_ref() != Some(&request.scope) {
        return unobserved("effective wrapper binding changed since probe selection");
    }
    let Some(scope) = child.probe_scope(&request.context) else {
        return unobserved("selected child has no probe scope");
    };
    let mut child_request = request.clone();
    child_request.expected.executor = scope.worker.clone();
    child_request.expected.inputs_digest =
        probe_inputs_digest(&request.action, &scope, &request.context);
    child_request.scope = scope;
    let observed = child.probe(&child_request).await;
    let matches = observed.operation == child_request.expected.operation
        && observed.executor == child_request.expected.executor
        && observed.inputs_digest == child_request.expected.inputs_digest
        && observed.generation == child_request.expected.generation
        && observed.run == child_request.expected.run;
    if !matches {
        return unobserved("child observation does not match the delegated binding");
    }
    request
        .expected
        .observation(observed.outcome, observed.observation)
}

impl ProbeRequest {
    /// Refuse inconsistent host requests before performing any operation.
    pub fn valid(&self) -> bool {
        valid_expected(&self.expected).is_ok()
            && self.expected.executor == self.scope.worker
            && self.expected.operation == self.action.operation()
            && digest(&self.scope.ceiling_digest)
            && digest(&self.scope.policy_digest)
            && self.expected.inputs_digest
                == probe_inputs_digest(&self.action, &self.scope, &self.context)
            && self
                .context
                .bound_dir
                .as_ref()
                .is_none_or(|p| p.is_absolute())
            && (self.context.bound_dir.is_none() || self.context.requirements.workspace.is_some())
            && self
                .context
                .requirements
                .workspace
                .as_ref()
                .is_none_or(|w| w.repo.is_absolute() && !w.base.trim().is_empty())
            && !self.deadline.is_zero()
            && self.deadline <= Duration::from_secs(30)
    }
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
    need: &ExpectedOperation,
) -> Result<Proof, Missing> {
    valid_expected(need)?;
    validate_latest(item, evidence, need)
}

/// Validate within the exact controller-selected generation. A shared capability
/// may serve several waiting items, whose generations do not compete. This
/// selection is meaningful only when `need` comes from the controller's durable
/// generation record. Malformed/oversized rows are retained and fail closed;
/// they cannot disappear merely because their generation cannot be read. Within
/// the selected generation the same latest/provenance/binding rules apply.
pub fn capability_proven_in_generation(
    item: &WorkItem,
    evidence: &[Evidence],
    need: &ExpectedOperation,
) -> Result<Proof, Missing> {
    valid_expected(need)?;
    let selected: Vec<_> = evidence
        .iter()
        .filter(|e| {
            e.kind == OPERATION_PROBE
                && (e.reference.len() > MAX_REFERENCE_BYTES
                    || serde_json::from_str::<OperationProbe>(&e.reference)
                        .map_or(true, |p| p.generation == need.generation))
        })
        .cloned()
        .collect();
    validate_latest(item, &selected, need)
}

fn valid_expected(need: &ExpectedOperation) -> Result<(), Missing> {
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
    Ok(())
}

fn validate_latest(
    item: &WorkItem,
    evidence: &[Evidence],
    need: &ExpectedOperation,
) -> Result<Proof, Missing> {
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

    #[test]
    fn one_registry_maps_needs_and_rejects_unregistered_invocations() {
        use crate::{errors::GapKind, routing::CapabilityRef};
        for gap in [GapKind::Tool, GapKind::Install] {
            for binary in ["cargo", "node"] {
                let action = registered_action(&CapabilityRef {
                    gap,
                    subject: binary.into(),
                })
                .unwrap();
                let call = action.tool_call("probe-id").unwrap();
                assert_eq!(call.id, "probe-id");
                assert_eq!(call.name, "exec");
                assert_eq!(
                    call.arguments,
                    serde_json::json!({"command":format!("{binary} --version"),"timeout_secs":30})
                );
            }
        }
        let action = registered_action(&CapabilityRef {
            gap: GapKind::Tool,
            subject: "work_resources".into(),
        })
        .unwrap();
        assert_eq!(action.tool_call("probe-id").unwrap().name, "work_resources");
        for (gap, subject) in [
            (GapKind::Install, "work_resources"),
            (GapKind::Tool, "unknown"),
            (GapKind::Tool, "node; touch forbidden"),
        ] {
            assert!(registered_action(&CapabilityRef {
                gap,
                subject: subject.into()
            })
            .is_none());
        }
        for action in [
            RegisteredAction::BinaryAvailable {
                binary: "node; touch forbidden".into(),
            },
            RegisteredAction::ToolCallable {
                tool: "work_resources".into(),
                args: serde_json::json!({"project_id":"other"}),
            },
            RegisteredAction::ToolCallable {
                tool: "node".into(),
                args: serde_json::json!({}),
            },
            RegisteredAction::BinaryAvailable {
                binary: "work_resources".into(),
            },
        ] {
            assert!(action.tool_call("probe-id").is_none());
        }
    }

    #[test]
    fn shared_capability_generations_do_not_hide_failure_or_malformed_receipts() {
        let (item, need, success) = fixture();
        let mut other = change(&success, |p| {
            p.generation = "another-waiter:8".into();
            p.run = "other-probe".into();
        });
        other.at += chrono::Duration::seconds(10);
        assert!(
            capability_proven_in_generation(&item, &[success.clone(), other.clone()], &need)
                .is_ok()
        );
        assert_eq!(
            capability_proven(&item, &[success.clone(), other.clone()], &need),
            Err(Missing::StaleGeneration)
        );
        assert_eq!(
            capability_proven_in_generation(&item, &[other.clone()], &need),
            Err(Missing::NoProbe)
        );
        let mut failed = change(&success, |p| p.outcome = ProbeOutcome::Failed);
        failed.at += chrono::Duration::seconds(1);
        assert_eq!(
            capability_proven_in_generation(
                &item,
                &[success.clone(), other.clone(), failed],
                &need
            ),
            Err(Missing::Failed)
        );
        for reference in ["{broken".into(), "x".repeat(MAX_REFERENCE_BYTES + 1)] {
            let invalid = Evidence {
                reference,
                ..other.clone()
            };
            assert_eq!(
                capability_proven_in_generation(&item, &[success.clone(), invalid], &need),
                Err(Missing::MalformedProbe)
            );
        }
        let untrusted = Evidence {
            at: other.at,
            verified_by: Some("model".into()),
            ..success.clone()
        };
        assert_eq!(
            capability_proven_in_generation(&item, &[success, untrusted], &need),
            Err(Missing::Untrusted)
        );
    }

    #[test]
    fn probe_request_binds_action_args_executor_and_effective_ceiling() {
        let names = vec!["exec".into(), "work_resources".into()];
        assert_eq!(
            tool_ceiling_digest(&names),
            tool_ceiling_digest(&["work_resources".into(), "exec".into(), "exec".into()])
        );
        let context = ProbeContext {
            requirements: EffectiveRequirements {
                kind: rustykrab_core::work::WorkKind::Task,
                required_tools: vec![],
                required_mcp_servers: vec![],
                writable_resources: vec![],
                read_only_review: false,
                workspace: None,
            },
            bound_dir: None,
        };
        let scope = ProbeScope {
            worker: "pinch".into(),
            path: ExecutionPath::Local,
            policy_digest: tool_ceiling_digest(&[]),
            ceiling_digest: tool_ceiling_digest(&names),
        };
        let action = RegisteredAction::ToolCallable {
            tool: "work_resources".into(),
            args: serde_json::json!({}),
        };
        let request = ProbeRequest {
            expected: ExpectedOperation {
                operation: action.operation(),
                executor: scope.worker.clone(),
                inputs_digest: probe_inputs_digest(&action, &scope, &context),
                generation: "original:2".into(),
                run: "probe-1".into(),
            },
            action,
            scope,
            context,
            deadline: Duration::from_secs(30),
        };
        assert!(request.valid());
        for change in 0..6 {
            let mut changed = request.clone();
            match change {
                0 => changed.scope.worker = "other".into(),
                1 => changed.expected.operation = "repository.read".into(),
                2 => changed.scope.ceiling_digest = tool_ceiling_digest(&[]),
                3 => {
                    changed.action = RegisteredAction::ToolCallable {
                        tool: "work_resources".into(),
                        args: serde_json::json!({"project_id":"other"}),
                    }
                }
                4 => changed.deadline = Duration::from_secs(31),
                _ => changed.context.bound_dir = Some("relative".into()),
            }
            assert!(!changed.valid(), "change {change}");
        }
        let mut moved = request.clone();
        moved.context.requirements.workspace = Some(WorkspaceIdentity {
            repo: "/fixture".into(),
            base: "base".into(),
        });
        moved.expected.inputs_digest =
            probe_inputs_digest(&moved.action, &moved.scope, &moved.context);
        moved.context.bound_dir = Some("/tmp/new-probe-worktree".into());
        assert!(
            moved.valid(),
            "the registered availability claims do not attest repository content"
        );
    }

    fn fixture() -> (WorkItem, ExpectedOperation, Evidence) {
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
        let need = ExpectedOperation {
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
    #[test]
    fn probe_identity_normalizes_sets_but_changes_with_policy_path_or_requirements() {
        let mut context = ProbeContext {
            requirements: EffectiveRequirements {
                kind: rustykrab_core::work::WorkKind::Task,
                required_tools: vec!["exec".into(), "read".into()],
                required_mcp_servers: vec!["fixture".into()],
                writable_resources: vec!["repo:/fixture".into()],
                read_only_review: false,
                workspace: Some(WorkspaceIdentity {
                    repo: "/fixture".into(),
                    base: "commit-a".into(),
                }),
            },
            bound_dir: Some("/tmp/first-probe".into()),
        };
        let scope = ProbeScope {
            worker: "same-worker".into(),
            path: ExecutionPath::Host,
            policy_digest: tool_ceiling_digest(&["policy-a".into()]),
            ceiling_digest: tool_ceiling_digest(&["exec".into()]),
        };
        let action = RegisteredAction::BinaryAvailable {
            binary: "node".into(),
        };
        let original = probe_inputs_digest(&action, &scope, &context);
        context.requirements.required_tools = vec!["read".into(), "exec".into(), "exec".into()];
        context
            .requirements
            .required_mcp_servers
            .push("fixture".into());
        context
            .requirements
            .writable_resources
            .push("repo:/fixture".into());
        context.bound_dir = Some("/tmp/different-invocation".into());
        assert_eq!(original, probe_inputs_digest(&action, &scope, &context));
        for case in 0..8 {
            let mut c = context.clone();
            let mut s = scope.clone();
            match case {
                0 => s.path = ExecutionPath::Native,
                1 => s.policy_digest = tool_ceiling_digest(&["policy-b".into()]),
                2 => c.requirements.workspace.as_mut().unwrap().base = "commit-b".into(),
                3 => c.requirements.read_only_review = true,
                4 => c.requirements.required_tools.push("work_resources".into()),
                5 => c.requirements.required_mcp_servers.push("different".into()),
                6 => c
                    .requirements
                    .writable_resources
                    .push("file:/different".into()),
                _ => s.worker = "another-worker".into(),
            }
            assert_ne!(
                original,
                probe_inputs_digest(&action, &s, &c),
                "binding change {case}"
            );
        }
    }
}
