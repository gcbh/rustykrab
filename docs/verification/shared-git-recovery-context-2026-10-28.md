# Shared recovery question-history projection

Exact base: deployed v6.6.12, `b5c0a82f48cc44b2dbf7dbd130f44613ced1914a`.

The live rollout allocated repair `e8402ec7-7ce7-4adc-a251-038ad4ef8e04`
for 36 waiters, then hit the existing 128 KiB execution-view limit before any
lease. Its relevant question history contained 101 obsolete unanswered rows
(110,237 JSON bytes) and 18 answered rows (18,363 bytes). Current work
definitions accounted for another 42,614 bytes. No worker had run this repair.

The projection change keeps every question in the durable context receipt and
database, but omits obsolete questions from the prompt when they contain no
answer, answer submission or delegated decision. Existing active questions,
owner input, goals and constraints retain their content. The size guard stays.

## Regression evidence

- The 36-waiter/101-question/18-owner-answer real-Git/SQLite regression fails
  on the exact deployed base: the shared repair is Blocked(NeedsDecision),
  rather than Done. The failing log is retained in the task artifacts as
  `shared-git-recovery-context-red.log`.
- After the change, one actual fixture worker receives a normal worktree,
  commits a resolution containing both parents, and all 36 originals execute
  and finish through the production controller. The repair receives every
  owner's answer and every waiter's definition. The full frozen context still
  exceeds 128 KiB, while the execution view is below it. Repository HEAD stays
  unchanged. The targeted suite passes all 15 Git recovery cases.
- A second regression retains open questions, answers even on obsolete rows,
  and pending answer submissions. Oversized active input still fails the
  original execution-context guard.

Required validation passed: formatting, workspace/all-target Clippy with
`-D warnings`, architecture regeneration/check, environment docs, and the full
workspace suite: 2,481 passed, zero failed, 39 ignored across 40 targets. The
15 targeted Git-recovery cases pass. Task receipts bind the exact reviewable
commit and full logs. No production question is answered, task manually requeued,
budget changed or live history pruned by this implementation. Promotion and
actual model effectiveness remain separate from deterministic execution proof.
