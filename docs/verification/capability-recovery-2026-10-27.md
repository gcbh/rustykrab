# Capability recovery: 2026-10-27

Base: `de3768413cd45a950709df29aa827fa0297bd1a3`, deployed v6.6.9.

## Observed failure

The owner answered item `39bcaa42-fb97-4b43-a03d-0d448c3f6ad7`'s missing-tool
question with permission to build a fix and open a PR. The frozen attempt
`c3cc92d6-4c36-4f4f-b29c-f5867b15573b` contains that decision, no writable
repository, and `required_tools:["capability:tool:work_resources"]`. Planner's
actual ceiling omits both that name and the real inventory tool. LocalWorker's
activation rejects absent required tools before its first model call or
transcript. No corresponding transcript exists. Zero reported token counts alone
would not establish that inference never started; the execution path supplies
the stronger evidence. The next state was blocked/needs_tool, without a repair.

The host has a real read-only `work_resources` ResourcesTool, plus a late
SKILL.md named `capability:tool:work_resources`. Catalog membership of that
instruction file does not implement the operation. A legacy capability artifact
`{"kind":"capability","value":"tool:capability:tool:work_resources"}` also
missed the registered functional-proof action and fell through to availability
only. A preflight rejection cannot be corrected by strengthening the unread prompt.

## Correction and regressions

- Decode the explicit serialized `capability:tool:` namespace at blocked need,
  filing/ref and historical activation/routing boundaries; retain historical
  rows and unrelated namespaces/colon names.
- Include the existing read-only inventory in the general worker's context
  ceiling. No new implementation tool, write scope or credential is granted.
- Match against the actual brief's effective tools, including recovery
  activations, and route/surface a catalog-known tool missing from an adapter's
  ceiling. Host binaries with registered version probes retain their exec path.
- Preserve registered functional proof, generation binding, independent owner
  holds and budget accounting. Legacy nested refs cannot bypass that gate.

On the unchanged production code, the two initial control regressions failed
because a nested ref had no registered probe and history kept the malformed
name. The real CLI adapter regression failed at the omitted inventory ceiling.
With the correction, the end-to-end fixture drives the actual Controller,
IntegrationWorker/LocalWorker and ResourcesTool using scripted inference and a
temporary store. It verifies one acquisition, a successful executor-bound
`tool.callable/work_resources` receipt, two leases of the original item, and
**two successful real inventory calls**: the host probe and the resumed original.
It runs without repository/write scope or external model/network access.

Additional control regressions refuse an Unobserved legacy done claim, preserve
an independent owner hold and unchanged generation, and route a local ceiling
mismatch only when the alternative covers the same resource scope. Missing or
scope-incompatible alternatives create one question, no acquisition chain and
no repeated unchanged events or budget grant. Three older scripted fixtures now
advertise the exact pdf_render tool when it is available; their existing build,
acquisition and availability-only assertions remain.

## Delivery boundary and remaining gaps

This source change and its fixtures establish implementation behavior. They do
not establish that this patch is landed, deployed, or effective on the owner's
live task. No live task was requeued, answered, funded or rewritten during this
repair. An existing hold still needs its applicable owner action after deployment.
Actual model choice and the original goal's acceptance need a subsequent live
observation. Native-selected paths still cannot substitute host proof.

A general bootstrap path for controller/composition faults, durable preflight
transcripts, conversion of an owner's repair instruction into a separately
scoped code assignment, and the automatic-start UI/API eligibility error remain
outside this patch. No natural-language permission parser or broad self-grant
is introduced.

## Validation

- `cargo fmt --all -- --check`: pass.
- `cargo clippy --workspace --all-targets -- -D warnings`: pass.
- `cargo test --workspace`: **2,465 passed, 0 failed, 39 ignored** across 39
  test/doc-test targets.
- `python3 scripts/check_architecture_docs.py --fix`, followed by its check:
  pass; control/CLI metrics regenerated and semantic recovery sections updated.
- `python3 scripts/check_env_docs.py`: pass.

Full checks ran outside the outer Codex sandbox so the existing temporary
loopback and nested Seatbelt fixtures could exercise their own boundaries.
An initial in-sandbox control run found two loopback-bind EPERM failures and
three scripted inventory/ceiling inconsistencies described above. All original
assertions remain; the corrected explicit tool fixtures and final full suite
pass. The focused real-adapter regression also passes with CLI default features
disabled. No live model, integration account or held work item was exercised.
