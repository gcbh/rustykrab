# Functional capability-proof boundary

Base: `619d476` (v6.5.7). This slice implements a pure evidence validator and
removes model-authored operation probes at both worker report boundaries.
It does not yet execute probes or replace the controller's catalog gate;
controller integration belongs to Claude's coordinated recovery slice.

## Contract

`rustykrab_control::capability_proof` exports `OPERATION_PROBE`,
`CapabilityRef`, `OperationProbe`, `ProbeOutcome`, `Proof`, `Missing` and:

```rust
pub fn capability_proven(
    item: &WorkItem,
    evidence: &[Evidence],
    need: &CapabilityRef,
) -> Result<Proof, Missing>
```

The controller constructs `CapabilityRef` from expected operation/executor,
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
