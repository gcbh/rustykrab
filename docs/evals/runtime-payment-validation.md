# Runtime continuity and payment-screen validation

Date: September 10, 2026 (Pacific). Base: `0b565fdbe80095668a402cdc6b5d410efecc867b`,
v5.2.38, plus the uncommitted `codex/broadway-context-eval` worktree changes.
**Not deployed. No production conversations, credentials or browser profile modified.**

## Verdict

**Partial verification.** The targeted runtime repairs and controlled payment-screen
regressions pass. A real local-model trial still takes a clock detour with complete
Broadway context. Reusable card enrollment/fill is **not implemented or certified safe**.
This is not a claim that all three historical incidents are resolved.

### Runtime fixes verified

- Compaction retains the latest real instruction verbatim, after the summary,
  even when an adversarial summarizer omits the correction. The original task is
  also retained; synthetic retry prompts cannot replace the pinned current input.
- Telegram now uses shared runner setup. Its configured active tools appear in
  captured requests, matching HTTP; loss of that activation is a failing eval.
- Provider-error completion returns partial history. The real Telegram adapter
  saves the user message and tool result to SQLite and reports the incomplete run.
- Mid-run input reaches the next provider request, including input arriving while
  the prior request will return EndTurn. Completion seals admission before exit.
  Failed/full inbox admission falls back to a serialized turn, not silent disposal.
- Cancellation interrupts a stuck provider and returns history. Parallel local tool
  tasks are aborted on drop; unfinished calls are recorded as **unknown outcome**,
  never blindly retried. Queued follow-ups do not reset the iteration budget.

Evidence: **22/22 deterministic daemon trials** (20 base cases/surfaces plus two
paired-explicit follow-up cases). Five runner regressions cover injection, error,
cancellation, parallel-task cleanup and budget exhaustion. A separate audit rehashed
and reparsed **60 exact wire exchanges** across deterministic and live-model trials,
and checked that native prompt traces matched the sequential wire requests.

### Live-model result: an unresolved interpretation problem

Same `gemma4:26b`, compact synthetic Broadway history, four-iteration cap, inert
domain tools and real Telegram/runner/Ollama paths. One trial per wording:

| Follow-up | Context contract | Observed action | Behavioral verdict |
|---|---|---|---|
| “Make use of the browser to fetch the time and date info” | Pass; original task, prior answer and latest input retained on every request; no compaction | Calls `datetime.now()` and fetches `timeanddate.com`; no Broadway-directed action | **Fail: clock drift**, 81.8 s |
| Explicitly continue Broadway show times/seating on September 14–16 | Pass | Navigates to Broadway.com and searches those Broadway dates; no clock action | **Pass: next-action alignment**, 53.9 s |

The failed run's final answer still mentions the Broadway objective. This is an
execution detour with context present, not evidence that the task vanished from the
request. These are newly captured reproductions, **not recovery of the original
Broadway request**. One pair does not establish a failure rate or prove an automatic
prompt remedy. Warm-cache differences make the elapsed times unsuitable as a speed
comparison. Neither run completes availability retrieval: domain tools intentionally
return controlled failures. The next fix needs generic active-task/reference handling
tested against explicit task switches and other domains, not a Broadway-specific rule.

### Payment-screen result

Real Chrome, RustyKrab BrowserTool/CDP, loopback checkout, synthetic values only.
No account, actual merchant or payment processor. The recorded form presents card
number, expiry, security code and **Pay USD 12.00**. Submission count stayed **zero**.

The pre-fix test demonstrated three defects:

1. Implicit HTML labels were missing from model-visible field names.
2. Text-type card number, expiry and CVV values appeared in ordinary snapshots.
3. Generic credential capture accepted an explicitly named card/CVV request.

After the fixes, the same test identifies the fields, omits recognized payment
values, rejects the generic card request, creates no pending credential form and
stores no secret. A separate store test rejects legacy payment requests at fulfilment
before any value is written. The checkout screenshot is taken **before synthetic
values are entered**, not after redaction, and has been visually inspected.

**Current experience:** the credential tool contract tells the agent to stop and
ask for direct entry on the trusted merchant page; it cannot claim a saved wallet.
The live browser test exercises the screen and tool/store policy, **not the model's
decision to ask the user**. No secure card enrollment or cross-site reuse was exercised.

Desired wallet flow and its blockers are in [the contract/design](payment-and-runtime-contract.md).
They include trusted out-of-model entry, explicit saving consent, a separate vault,
per-merchant permission, frame-origin checks, metadata-only results and no retained
CVV. Ordinary credential capture and snapshot redaction alone are insufficient.

## Retained artifacts

All paths below are in this persistent worktree, not the former temporary directory.
They are ignored build artifacts; `cargo clean` would remove them.

- [20 deterministic cases](../../target/e2e-artifacts/context-0f12e995-c987-4ae3-a2ea-fb4c11bbaee5/report.json)
- [Two paired-fixture cases](../../target/e2e-artifacts/context-8e55f8a5-5a22-4172-b55e-794e7a662eb0/report.json)
- [Live ambiguous follow-up: failed](../../target/e2e-artifacts/context-aba15c28-ba71-4792-b579-32489f76cd3c/report.json)
- [Live explicit follow-up: aligned](../../target/e2e-artifacts/context-2fe172f5-4ae5-435c-a6a7-cadec6e07e71/report.json)
- [Payment pre-fix flags](../../target/e2e-artifacts/payment/payment-result-before.json)
- [Payment post-fix flags](../../target/e2e-artifacts/payment/payment-result-after.json)
- [Checkout screenshot](../../target/e2e-artifacts/payment/checkout-screen.png)
- [Workspace tests](../../target/runtime-regression-tests.log), [clippy](../../target/runtime-clippy.log)

Each context directory also contains a manifest with tested daemon/evaluator hashes,
fixture-source hash and, for live runs, model metadata/template; individual trial
files retain wire bodies, prompt traces, final SQLite readback and channel replies.
Raw model reasoning and real secrets are not retained.

## Checks and remaining uncertainty

- Workspace `cargo test --offline --workspace --no-default-features`: **1,031 passed**,
  31 normally ignored. The payment Chrome test was separately run explicitly and passed.
- Workspace/all-target `cargo clippy --offline --no-default-features`: clean.
- `cargo fmt --all -- --check`, architecture generation/check and `git diff --check`: pass.
- No-default-features uses the documented non-ONNX path. Production embeddings,
  authenticated Instagram/United flows and Google Flights were **not revalidated** here.
- Provider-side trimming can still remove older objectives in a small window; the
  trim control documents this, not a fix. Historical exact incident payloads remain unavailable.
- Hard process death before a queued message is saved, reset races, Slack lifecycle
  parity, full mid-run distillation and durable completion of async memory hooks remain open.
- Recognized-field redaction is not comprehensive secret isolation. Arbitrary evaluate,
  screenshots, mirrored DOM text, profile data and caller-selected credential origins
  require a stronger boundary before real card storage/reuse. The field-name guard can
  also conservatively reject non-payment fields called “security code”.

Architecture updated: agent, CLI, runtime, gateway, tools, store and e2e crate write-ups;
system overview; opinion follow-up and outcome history. No new dependency, table or
payment-vault API. The six-copy turn-transaction finding remains open.
