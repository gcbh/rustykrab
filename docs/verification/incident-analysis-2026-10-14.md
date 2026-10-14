# Frozen operational inputs and local review boundary

Base: b2b0946 (#694) over main 619d476. Branch codex/incident-analysis-inputs.
No live daemon change, merge, deployment or incident scheduling claim.

## Problem and behavior

Project reviews previously received repository text and only directly tagged
work objectives/statuses. They could miss untagged project descendants, recorded
spend, dependencies and failures; large acceptance text could overflow input.
The general scheduler can now select Qwen for these research jobs, yet only
native adapters enforced the read-only marker.

The local adapter now enforces a report-only catalog/ceiling, no automatic
recall and no contradictory execution scope. Its early recording backend
rejects authority-bearing result fields. Normal work retains its capabilities.

Both review stages receive the same bounded controller-operations observation:
complete unarchived project kind/status counts using ancestor bindings;
dated daemon/controller and global ledger facts; up to 12 sampled project
items, recent event metadata and durable worker advertisements. Dependency/tool
samples have explicit counts; recent failures include class/subclass/fingerprint
only. Raw traces, outputs, private configuration, credentials and exact file
grants are omitted. The 500-item/80-event monitor bounds, sample coverage and
missing observations are explicit. Each observation is valid JSON at most
12 KB. Text work rows are at most 30/20 KB, with 1,200-byte objective/acceptance
fields; total input remains at most 100 KB.

These are observations, not original-goal throughput, trusted functional proof
or current dispatch capacity. Kind/status counts include repair/parent rows;
zero tokens may mean missing accounting. Archived work is counted globally
but not attributed to a project. Freshness starts before collection and
publication uses the same inherited scope. Conflicting project identities are
excluded, not silently assigned.

## Verification

Six new regressions exercise real file/store boundaries, scripted model calls,
ancestor inheritance, independent holds, recorded spend, typed failure metadata,
private-trace omission, immutable later reads, population versus sample counts,
multibyte oversized text and missing durable worker advertisements. Existing
Git/SQLite project-review and exact citation/calibration/crash recovery tests
also run. Final restored-source workspace: 2,241 passed, zero failed, 31 ignored
(one test thread). Workspace/all-target clippy with -D warnings, format and
both documentation checkers passed. Removing read-only enforcement makes
the real write fixture fail; exact source restored before final verification.
A preceding two-thread run passed, while a subsequent loaded run timed out
in the existing overseer completion fixture (Running instead of Done).
No unrelated fixture was modified. Build targets are isolated per branch
after concurrent shared-target interfaces proved incompatible. No live model trial or diagnostic-quality claim.

## Remaining integration

The existing foreground gate still treats queued/ready presence as demand.
A separate controller dispatchability observation and incident-availability
policy are being coordinated with Claude. The controller-wide read-only report
guard is also its owned slice; this branch protects the local early-forwarding
path. #695's capability/probe/trait work is separate.
