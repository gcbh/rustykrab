# Native command diagnostic observations

Base: #707 `b97221be8deaba4517c6e7a809efeb19c521ac23`.

Native workers already retain raw runtime event streams. Their normalized
transcript keeps command strings, so an investigator cannot read a compact,
structured result for each command. Claude Bash tool-use and legacy Codex begin
events also describe a request rather than a completed operation.

This change appends one diagnostic summary to the existing private run log for
each owned subprocess capture, even when no model report is produced. It changes
no command launch, scheduler, budget, report artifact, check verifier or recovery
gate. Native `probe()` remains Unobserved and launches nothing.

## Receipt shape

The existing `rustykrab.invocation` marker and a new
`rustykrab.native_observations` summary share a host-generated invocation UUID.
The summary carries `worker`, `run`, invocation label, runtime kind and `observed`:

```json
{
  "schema_version": 1,
  "diagnostic_only": true,
  "capture_incomplete": false,
  "observations": [
    {
      "session": "runtime-thread",
      "correlation": "runtime-command-id",
      "command": {"text": "node --version", "original_bytes": 14, "truncated": false},
      "decoded_command": {"text": "node --version", "original_bytes": 14, "truncated": false},
      "outcome": {"state": "completed"},
      "runtime_status": {"text": "completed", "original_bytes": 9, "truncated": false},
      "exit_code": 0,
      "stdout": {"text": "v22.0.0", "original_bytes": 7, "truncated": false},
      "stderr": null,
      "result_text": null
    }
  ],
  "omissions": {"input_bytes": 0, "oversized_lines": 0, "malformed_lines": 0, "records": 0}
}
```

The example is synthetic, not an observed CLI run. Outcomes are `requested`,
`completed`, `failed`, `interrupted` or `unobserved` with an explicit reason.
Completion requires a paired modern Codex terminal status and numeric exit zero.
Nonzero exits retain failure; missing, contradictory, duplicate, reordered or
changed-command observations remain unobserved. Session/thread and invocation
boundaries cannot pair. Legacy Codex begin stays a request.

Claude results retain raw stdout/stderr when the raw object belongs to one
tool-result block, and string result content for diagnosis (including refusal
messages). Runtime `is_error` and `interrupted` are recorded. Numeric exit
semantics remain unestablished; neither success prose nor an extra numeric field
is promoted into completion. JSON embedded in model text or output is never
parsed as a runtime event.

At most 4 MiB of input, 256 KiB per JSON line, 32 records, 256 bytes per identity
and 1,024 UTF-8-safe bytes per text field are retained. Omissions and truncation
are explicit. Partial, malformed or bounded-away capture cannot leave a
successful-completion observation. Existing raw logs are preserved beside the
summary, under the same 0600 file and retention policy; no new public surface is
added. This is additional bounded diagnostic data, not a context prompt dump.

These fields retain raw tool output and can contain sensitive material printed
by a command. They remain in the existing private session log. The current
investigation kit grants read access to that log; it redacts separate daemon-log
slices, rather than redacting session-log contents. This change introduces no
export or redaction guarantee.

## Proof boundary

The private parser consumes only stdout from an adapter-owned child capture.
It does not read saved logs or model artifacts. Its output is diagnostic only;
the log remains mutable investigation material. There is no `operation_probe`
production, no model-derived authority and no native-success revival path here.

Future native proof needs a separately agreed trusted live adapter seam, a
controller-owned pre-execution challenge, actual account/path/policy/requirements
and workspace binding, registered postcondition, and finite budgeted dispatch
through the existing graph. Command completion alone proves neither registered
availability nor a build, source landing, deployed repair or original goal.
Claude remains Unobserved until its runtime exit contract is established.

## Validation

Parser regressions cover numeric/boolean/missing exits, status contradictions,
missing/reordered/duplicate ids, command mismatch, invocation/thread/session
boundaries, request-only formats, model text imitation, Claude failure and
interruption, partial/malformed capture, invalid identities and bounded Unicode
output. Adapter regressions exercise a real scripted child failing without a
report, timeout capture, private file mode and distinct resumed invocation ids.
These scripted subprocesses are not live Claude/Codex model or account trials.

Final gate counts and negative-control evidence are recorded in the task's
`antifragile-collaboration/native-outcome-verification.json` receipt. Architecture
prose and generated counts are updated in `crates/rustykrab-agent/ARCHITECTURE.md`;
no crate dependency, shared trait, persistent table or controller contract changes.
