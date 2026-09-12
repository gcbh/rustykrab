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
  PAN detection. No reusable payment vault or CVV retention is enabled.

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
