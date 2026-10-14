# Functional capability-proof boundary

Base: `619d476` (v6.5.7). This slice implements a pure evidence validator and
removes model-authored operation probes at both worker report boundaries.
A follow-up implements registered local host producers; controller dispatch
and replacement of the catalog gate remain integration work;
controller integration belongs to Claude's coordinated recovery slice.

## Contract

`rustykrab_control::capability_proof` exports `OPERATION_PROBE`,
`ExpectedOperation`, `OperationProbe`, `ProbeOutcome`, `Proof`, `Missing` and:

```rust
pub fn capability_proven(
    item: &WorkItem,
    evidence: &[Evidence],
    need: &ExpectedOperation,
) -> Result<Proof, Missing>
```

The controller constructs `ExpectedOperation` from expected operation/executor,
canonical lowercase SHA-256 over inputs plus effective access/configuration,
the relevant repair/configuration generation and the host probe invocation
`run`. Secret values and incidental invocation paths stay out of that digest.
Expected bindings are not selected from the model's proposed probe.

The adapter/host writes a generic existing evidence row with:

- `item`: the capability work item whose required operation was exercised;
- `kind`: `operation_probe`;
- `reference`: serialized `OperationProbe`;
- `hash`: the same inputs/access digest;
- `verified_by`: `executor:<expected worker name>`;
- `at`: the actual observation time.

The reference has exactly these fields:

```json
{
  "operation": "repository.read",
  "executor": "max-fieldkit",
  "inputs_digest": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "generation": "repair-2/access-4/build-619d476",
  "run": "probe-2",
  "outcome": "ok",
  "observation": "host-trace:probe-2/read-1; exit=0; expected bytes matched"
}
```

This example is a synthetic fixture, not a receipt of live execution.
Outcomes are `ok`, `failed` and `unobserved`; only a matching `ok` observation
can construct `Proof`. It has private fields and no deserializer. Callers
inspect `proof.probe()` and `proof.observed_at()`.

The latest operation-probe row for the item governs. Older success cannot
hide a newer failure, missing result, malformed observation or changed binding.
Conflicting latest records at the same timestamp are ambiguous; exact duplicates
are idempotent. Rows from another work item, wrong executor/operation/input/run,
stale generations, missing host provenance, unknown/missing JSON fields and
oversized records are rejected. References are bounded to 16 KiB, observations
to 4 KiB; expected identity fields are nonempty, bounded and free of control
characters. The evidence must not predate the work item's creation.

`Missing` supplies a typed reason without copying private observation content.
The validator changes no status, budget, question, scope or consent. Its caller
must retain independent holds, #693's fresh-completion gate, dependencies and
resource matching. A successful probe is not completion of the owner's goal.

## Provenance boundary

A verifier string does not authenticate arbitrary caller-supplied JSON. The
pure validator assumes host-owned storage and controller-selected expectations.
Producers must derive probe outcomes from real adapter observations, not from
the model summary or a deserialized model artifact.

`ExternalWorker::attest` removes model-written `operation_probe` alongside
`command_run` before adding host-observed command records. The local
`RecordingBackend::report` does the same before forwarding the early report to
the controller and before saving its return receipt. Ordinary artifacts remain.
Neither adapter currently invents a probe or turns a command record into one.
Trusted probe production must be added through a separate host observation path.

## Verification scope

Regressions cover catalog-only/model claims, all expected bindings, empty or
malformed evidence, old-success/new-failure selection in either storage order,
ambiguous ties, exact duplicates and preservation of an independent owner hold.
Real SQLite reopen checks preserve valid bindings and a later failure across
restart. Native report sanitization preserves observed commands/ordinary
artifacts; the local test executes a real host command and inspects the exact
early-forwarded report and returned receipt.

These checks prove the validation and report boundaries. They do not establish
live model diagnostic quality, host probe execution, original-goal completion,
controller revival integration or deployed recovery. The shared anti-fragility
plan retains those steps as separate work.

## Local execution evidence

Workspace regression run: `cargo test --offline --workspace -- --test-threads=2`
completed with 2,242 passed, zero failed and 31 ignored. An earlier default-
parallel run passed all new proof/boundary checks but two existing controller
Git scenarios were still Running at their Done assertion. Reduced test
concurrency completed them; their implementation was not changed in this slice.

Two fault controls temporarily disabled only the new probe-artifact filters.
The native sanitization regression and the local real-command/early-forwarding
regression each failed their assertion (exit 101, one failed test). The original
source was restored and the full regression run above passed. Logs remain in
the operator's task workspace; these fixtures contain no live account data.

## Registered producer follow-up

The shared contract now exports `ProbeRequest { expected, action, scope,
deadline }`, `RegisteredAction`, `ProbeScope { worker, bound_dir, ceiling_digest }`,
`tool_ceiling_digest` and `probe_inputs_digest`. Deadline uses std::time::Duration
and is strictly positive, at most 30 seconds (including local-slot wait).
Scope worker equals ExpectedOperation.executor; registered action determines
ExpectedOperation.operation; inputs_digest must match the canonical request.
The controller selects the directory and persists the relevant generation before
dispatch. `LocalWorker::probe_scope` supplies its current effective ceiling.

Registered actions and their only claims:

- `BinaryAvailable { binary: "cargo" | "node" }`: exact fixed --version via
  the worker's existing exec tool. Identifiers `binary.available/cargo` and
  `binary.available/node`. Exit 0 plus `cargo <major>.<minor>...` or
  `v<major>.<minor>...` is required. An unrelated exit-0 response is failure.
- `ToolCallable { tool: "work_resources", args: {} }`: identifier
  `tool.callable/work_resources`; success requires an object with workers array.
  Other args, tool names and commands are not executed.

These claims establish availability/callability only. They do not prove a
repository read, compilation, test pass, permission for an unrelated command
or owner-goal completion. The digest binds action/args, worker and the sorted,
deduplicated effective tool ceiling. Worktree path and deadline are excluded
for these repository-independent claims; future content operations need a
separately registered content/access binding. Permission/configuration change
still requires the controller's fresh generation. No fallback to version/describe
may satisfy a different original functional claim.

LocalWorker::observe_probe reuses runner::execute_single_tool, its schema and
sandbox checks and the existing shared model/resource slot. ExternalWorker's
same method returns Unobserved for native Claude/Codex; it invokes no process
or model and makes no claim about a native runtime's access. Fixed receipts
omit raw tool outputs, stdout/stderr and private resource data. Both methods
return observation data only; only controller-owned storage plus the pure
validator may construct Proof. Worker trait overrides are now wired; lease dispatch, durable receipts and
capability judgment/revival remain Claude's integration slice.

Exec cancellation now kills its own process group on Unix, including ordinary
descendants. A real Python/sleep fixture verifies cancellation. Deliberately
detached processes are outside that group; this adds no OS access isolation.

Final restored-source verification: 2,251 workspace tests passed, zero failed,
31 ignored (two test threads). Workspace/all-target clippy with -D warnings,
format and both documentation checkers passed. Nine additional regressions
cover request/digest bindings, both registered postconditions, local execution,
no model/report/private-output leakage, invalid/unsupported requests, shared
slot and execution deadlines, sandbox/schema failures, native Unobserved and
real Unix subprocess-group cancellation. Disabling the postcondition check
and the process-group guard separately makes their regression fail (exit 101);
exact sources were restored before this complete final run.

Observed local environment gap during the follow-up: cargo --version through
the real exec boundary exited 1. Its cleared environment sets
HOME=/tmp/rustykrab-home; the rustup shim attempted a toolchain update rather
than using the owner's configured toolchain. A real Node version call passed
in the same boundary and is the positive execution fixture. Cargo remains
registered and must report Failed when this boundary cannot execute it.
No live daemon/toolchain setting was changed. This observation is specific
to this host setup; it does not establish Cargo failure on every deployment.

## Worker trait integration

Reviewed/cherry-picked Claude's 18a9129 seam (local cherry-pick b77f26a).
Local Worker::probe/probe_scope delegate to the existing host producer/scope
snapshot; both native adapters' dynamic Worker::probe remains Unobserved.
Local access_profile includes capabilities, repository_shell and sorted exact
readable_files grants, closing the bare-local-worker access-digest omission.
An Arc<dyn Worker> regression calls work_resources without model inference and
checks the actual scope/profile; the native regression now uses dynamic trait
dispatch too. Default implementations report unknown rather than proof.

Full workspace after wiring: 2,253 passed, zero failed, 31 ignored with two
test threads; warnings-denied workspace/all-target clippy, formatting and both
documentation checks passed. The controller has not yet requested/stored these
observations or replaced its catalog gate. No live deployment is claimed.
