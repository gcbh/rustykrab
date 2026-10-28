# Worker autonomy and bounded recovery — 2026-10-28

Base: exact main/deployed `e27efed4dabebb1aa5ff2a16ffc743542f6a70d9`.
Branch: `codex/worker-autonomy-recovery-20261028`.

## Observed failure and implementation

The deployed shared Git repair started successfully but only read source. Its
31 successful exec calls included the same diff twelve times, with no edit or
commit. It timed out after 1800 seconds with 1,603,490 reported tokens against a
200,000-token allowance. The copied planning-only constraint was contradictory;
the transcript does not prove it caused the model's behavior.

The repair now owns its execution constraints instead of inheriting the first
waiter's task-local contract. Fresh leases of existing repairs recognize only the
exact old controller boundary appended to the unchanged source constraints under
a trusted recovery record. That brief gets the independent execution contract;
added/edited repair-specific restrictions and untrusted markers are preserved,
and stored history or active/resumable runs are never rewritten. Budget admission
still precedes this projection, so an existing spent repair remains held. Original constraints and saved owner direction stay
attributed in project context. A local worker explicitly owns planning, execution
and verification within its assignment. Explicit graph-planning and read-only
review contracts remain distinct and respected.

After a warning at N unchanged tool results, the worker has another N unchanged
steps to change approach. Fresh arguments or results reset this fingerprint
counter. Persistent repetition becomes a typed model/loop before another model
request; further tool starts are refused, although already in-flight calls can
finish. The existing finite ladder prefers another compatible worker before
same-worker repair. It remains a deterministic fallback, not a new reasoning
manager service or a semantic measure of goal progress.

The metered provider checks all six request paths, including streaming and
compaction, against the brief's remaining token allowance. It records full actual
usage, including an in-flight request's overshoot, then refuses later requests.
Zero remains unbounded. Context-window capacity is separate from cumulative work
spending. No budget replenishment, new permission or authority bypass is added.

## Regressions and evidence

Task-local artifacts are retained under `antifragile-collaboration/` in the Codex
workspace. The initial targeted negative control fails on all four changed
behaviors at the deployed base (`worker-autonomy-red.log`): copied plan-only
contract, persistent repetition, token admission and worker-switch ordering.
The worker's opportunity to change course already passes at that base.

The actual agent integration fixture uses LocalWorker, ExecTool, its macOS
sandbox, controller, SQLite and real Git worktrees. One case changes course after
a warning, resolves both parent histories, commits and completes the original
useful work in a successor workspace. Another persistent loop ends after five
100-token model responses with a test threshold of two, then a deterministic
alternate Worker receives the normal failure evidence and reduced remaining
budget, resolves the real conflict, and the original local worker resumes and
commits its result. Both-parent commits and the original result are independently
checked; the source checkout/main never moves. No owner answer or grant is used.
The alternate is a Worker fixture of Codex kind, not a native Codex process.

The same handoff integration test fails against the exact five deployed
production file bodies, timing out without original completion. All current
file bytes are restored afterward and their SHA-256 values recorded in
`worker-autonomy-base-negative.json`; `worker-autonomy-integration-base-negative.log`
retains the failure. An earlier outer-sandbox run could not apply RustyKrab's
nested macOS sandbox; the successful fixture runs use the normal host boundary.
That harness failure is retained and is not a successful execution claim.

Additional cases cover every metered request path, zero/unbounded tokens, reset
on fresh evidence, a worker finishing after a warning, a multi-call batch stopped
at the recovery boundary, original task-constraint preservation, finite switch
ordering, guarded legacy contract projection and existing shared-repair/owner-hold guards.

## Validation status and limits

All 12 focused regressions, including both real-Git integration paths, pass. The first full workspace run found one existing standalone test assuming
same-worker repair before a switch; it is aligned with the approved new order,
and the adjacent no-alternative repair-evidence case remains unchanged. Other
failures from that run are absent. Final required checks pass: **2,492 tests passed, 0 failed, 39 ignored across
41 targets**, formatting, workspace/all-target Clippy with warnings denied,
architecture regeneration/check and environment documentation. Exact published
head and immutable log digests are retained in the task-local verification
receipt. Models are deterministic fixtures: useful live model reasoning
and native subscription execution are unmeasured. No live work was rewritten,
answered, granted budget, retried or restarted. The existing e840 repair remains
budget-held; this change neither clears that hold nor allocates a replacement
repair identity. No merge or deployment is part of this implementation run.
