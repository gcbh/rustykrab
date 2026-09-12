# Payment and interactive-runtime verification contract

Base: `0b565fdbe80095668a402cdc6b5d410efecc867b` (v5.2.38). Worktree changes are not deployed.

## Before execution: scope and uncertainty

The runtime contract is that the newest real user correction survives lossy
compaction; interactive channels share tool activation, recall and instrumentation;
accepted follow-ups reach a model dispatch or remain recorded as pending on error;
and provider errors/cancellation return partial history for persistence. Cancellation
cannot roll back external effects. Interrupted tools must report unknown outcomes,
not trigger automatic re-execution. Iteration budgets remain bounded across follow-ups.

The payment contract uses a controlled checkout with synthetic card fields and a
final Pay button. No real card, account, booking, merchant or payment processor is
used. The real Rust CDP stack must expose the screen, omit sensitive field values
from ordinary snapshots, and leave submission count at zero. Generic credential
capture must not offer to save a named card number/CVV as a login credential.

Uncertainties to retain in the result:

- Controlled browser/tool checks do not measure a local model's judgment.
- Snapshot redaction is not a secret-isolation boundary: arbitrary JavaScript,
  screenshots, page text mirrors and browser profile persistence need separate controls.
- Existing `fill_credential` can derive its lookup key from a caller-supplied URL,
  instead of exclusively binding to the current page/frame origin. This remains
  a blocker for a payment broker; this iteration does not certify that API as
  an origin-isolated secret boundary. Host-only key naming also merges ports/schemes.
- Field-name rejection is defense in depth, not a payment-data classifier. Arbitrarily
  disguised fields/values cannot be classified reliably this way.
- This does not implement a reusable wallet or claim PCI compliance. No real card
  should be enrolled until a dedicated vault and brokered-fill boundary exist.
- Full process death, hard abort and unsaved queued messages remain different from
  a normally returned provider error/cancellation; no exactly-once guarantee is claimed.
- Mid-run injections reach the memory write-back hook, but do not run the full
  channel `ingest_inbound` distillation path. The recall/outcome/activity wiring is
  code-checked; durable completion of every asynchronous memory hook is not proven.
- The original Broadway, Hyannis and FBAR provider requests were not retained, so
  these are regression reconstructions, not exact historical replays.

## Intended reusable-card experience (not yet implemented)

1. The agent reaches checkout and requests a payment method, naming the actual
   merchant origin and amount/currency when observed. It never requests PAN/CVV in chat.
2. A trusted app-owned sheet offers saved cards by nickname/brand/last four or secure
   enrollment. The user explicitly opts in to saving a card; values never pass through
   model messages, tool arguments, prompt logs or conversation history.
3. Card number and expiry live in a dedicated hardware-backed vault, inaccessible to
   generic credential reads and writes. CVV is per-use, transient and never saved.
4. A fill broker checks the current top-level origin, approved payment-frame origins
   and target field identities at use time. A new merchant needs fresh user approval.
   Saved credentials are not blanket authority to spend money.
5. Fill returns metadata only. Sensitive-page observation channels are gated, including
   screenshots, evaluation, DOM extraction, network logs and traces. Submit remains a
   separate explicit purchase authorization; this eval stops before it.

Tradeoff: merchant-specific PSP tokens cannot be assumed to work as general website
autofill. A cross-website wallet is a materially different security boundary from
the current host-keyed username/password store. It needs its own threat model,
revocation/deletion UX, expiry/3DS handling and hostile-iframe tests.

CVV retention restriction: [PCI SSC FAQ 1574](https://www.pcisecuritystandards.org/faqs/1574/).
Synthetic test data guidance: [Stripe testing](https://docs.stripe.com/testing).
