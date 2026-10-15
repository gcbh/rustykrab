# Dreaming admission display contract

Base: #705 `e78da2e714f3f20842e2e9de071b8c1e4ffd7401`.

The driver checked a literal three-cycle cap twice, while the core explanation
and dashboard each described that value independently. A future policy edit
could therefore leave the owner's explanation inconsistent with actual review
admission. The new core constant `PROJECT_REVIEW_DAILY_LIMIT` supplies both
driver checks, the generated explanation and the observed numeric limit.
Its value stays three cycles per rolling 24 hours.

`DreamingView` adds optional, defaulted `admission_explanation`, `admission_allows_review` and
`daily_review_limit`. The driver computes the explanation after finalizing the
typed admission decision. These presentation values do not alter capacity,
due-project selection, manual requests, cumulative budgets or owner consent.
There is no persistence migration or new endpoint.

The server also reports availability from the final admission result. The
Systems view labels the explanation as Review available or Review deferred
only for an observed boolean. Missing or malformed verdicts are explicitly
unobserved; neither prose nor a second browser decision table supplies them.
This availability snapshot is not a reservation, running cycle or permission.

The Systems view prefers the server's explanation using literal text and
reports its positive integer limit. Older servers retain the reason fallback;
their numeric limit is unobserved. Missing, blank or malformed explanations
fall back safely, and an explanation without an admission decision is ignored.
Future decision variants can provide their own text. Refresh clears old values.

Verification extends the real-store daily-cap reconstruction test and older
view deserialization test, plus the shipped-renderer fixtures for server text,
future variants, literal markup, unknown/malformed limits and refresh behavior.
The 15 focused real-store dreaming tests pass. All seven browser suites pass
44 tests, including eleven admission-renderer cases. Three in-memory defect
controls leave working source untouched: disabling the server explanation fails
three renderer checks, hiding the numeric limit fails one, and removing the
availability verdict fails two. The full workspace
passes 2,333 tests with zero failures and 32 ignored, one test thread. Strict
workspace/all-target lint, formatting, architecture/environment checks and four
environment-checker regressions pass.

Local checks bind the tested working Rust/JS sources on the pinned base to
hashes in the Codex task admission-verdict-verification.json. They do not claim
the local test binary was stamped with the later publication commit. Hosted
checks run independently after publication.

This is separate from conversation compaction-policy rollout and native
execution-path capability proof. No live configuration or daemon is changed.
