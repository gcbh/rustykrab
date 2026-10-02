# Credential and payment-boundary split

Final replacement for PR #642. This layer contains credential/browser security
changes, their tests and architecture notes; it sits above context/evaluation
to keep each review diff independent.

## Contract

Before a vault read, secure fill verifies the live top-level origin, target
frame and input type. A model-supplied URL cannot select a different credential.
Passwords require password inputs. Cross-origin frames and recognized payment
fields are rejected. Application targets the verified input object and rechecks
origin/type/attachment rather than relying on focus.

Snapshots use native implicit labels and omit recognized card/CVV field values.
Generic credential capture rejects explicitly named payment fields both at
filing and fulfilment of older pending forms.

## Compatibility and limits

- Exact-origin v2 keys include scheme, host and effective port. No hostname-only
  or legacy Instagram fallback remains. Existing keys are not deleted, but
  login credentials require re-enrollment; migration is not automatic.
- HTTPS is required except for explicitly configured private-network tests.
- Cross-origin SSO iframe flows are blocked; real framework/login compatibility
  needs separate validation.
- Secure fill omits automatic post-action snapshots. An unknown outcome does
  not justify blind retry after a potentially applied external effect.
- A same-origin page can read or mirror its input. Arbitrary evaluate,
  screenshots and reflected-page content are not isolated secret channels.
- Payment detection is conservative naming/autocomplete logic, not universal
  PAN detection. Generic credential capture still refuses card fields; cards
  use the separate one-purchase payment path below. No reusable payment vault
  or CVV retention is enabled.

## Payment path (added after this split)

A card is consent to one purchase, so it does not go through credential
capture. The user approves the terms (merchant, exact origin, maximum amount)
on a one-time `/p/{token}` page; the card is held in memory only and erased on
pay, after 15 minutes, on supersede, or on restart. `browser` `fill_payment`
enters one part per call into a field whose top origin is the approved merchant
and whose frame chain is the merchant or a fixed payment-provider allowlist.
`pay` refuses unless the page shows a total in the approved currency at or below
the approval, and spends the approval when pressed. It also refuses before
claiming anything unless the page is armed by an earlier `fill_payment` for the
approval that currently stands, so an approval is never spent pressing a
checkout with no card in it. While a card is on the page,
evaluate, screenshots, PDF, HTML content, coordinate clicks, Enter, and plain
clicks on submit-like controls are refused — including controls inside a
site-isolated frame, which `pay` cannot total-check either, so that checkout is
handed back to the user. The model-facing text of these refusals travels under
a `guidance` key that the agent runner exempts from its external-content fence;
everything page-derived stays fenced.

Evidence: `live_approved_card_is_entered_only_where_approved_and_paid_within_the_total`
(real Chrome, merchant/provider/ad on separate loopback origins with the provider
frame site-isolated, public synthetic card). 28 checks passed, covering refusal
without approval, outside the conversation, with a model-supplied value, into
the wrong field, and into a provider frame nested under a foreign origin; correct
landing of all six parts across the merchant page and provider frame; no card
value in results or snapshots; the armed-page refusals, including a click on and
a `pay` at the provider frame's own submit button; refusal of a `pay` before any
card was entered and of one whose fill a newer approval superseded, each with the
request left `authorized` and zero submissions; refusal of a total above
the approval with zero submissions; exactly one submission at the approved
total; and refusal of a second pay. Limits: loopback HTTP and a fixture provider
origin replace HTTPS and the production allowlist; no real merchant, processor
or model; total and submit detection are heuristics that fail closed.

## Evidence

[Pinned original captures and checksums](evidence.md) preserve the combined
build's real Chrome result: four invalid targets blocked before vault reads,
successful object-bound synthetic password fill, mutation recheck, no focus
leakage, no secret in the direct result/snapshot, and zero submissions.
This used isolated loopback origins and a synthetic secret backend, not merchant
credentials or production profiles.

The captures retain their original build identity. Splitting the PR does not
create a new live Chrome experiment; final-stack source equivalence and fresh
workspace checks are reported separately.

## Fresh split validation

- All application source, scripts, workflow and dependency files in the final
  stack match original commit `5790e24` exactly.
- Workspace tests: 1,055 passed, 30 ignored. Workspace/all-target clippy,
  formatting, architecture generation/checks and whitespace checks pass using
  the documented offline/no-default-features path (not default ONNX embeddings).
- Rebuilt real daemon: all 26 deterministic context cases/surfaces pass with
  explicit message-tail policy; scripted HTTP/SSE has 23 passes, six existing
  expected failures and no unexpected passes.
- These fresh runs use controlled model/domain fixtures. The original live
  Chrome captures were not rerun during splitting. No real accounts, merchant
  submissions or production credentials were used.
