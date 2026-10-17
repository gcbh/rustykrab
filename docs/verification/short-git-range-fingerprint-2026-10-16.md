# Stable short Git revision ranges in failure fingerprints

Base: `0441b56c4cb3ccd20d1b50f877c3c02c7592dca4` (v6.6.2).

The workspace verifier includes a `12hex..12hex` range in a claim-mismatch
detail. The normalizer previously treated a mixed hexadecimal revision as
`<id>`, a numeric-only revision as `<n>`, and a letter-only revision as a
literal word. Changing only a valid Git revision's spelling could therefore
change the failure fingerprint and split otherwise identical recurrence
history.

The new normalizer recognizes the whole short-revision range and uses
`<id>..<id>`. This retains the existing common mixed-hexadecimal identity.
Paths, URLs and quoted strings keep precedence; punctuation is retained.
No classifier, controller, workspace or controller-test source is changed.

## Deterministic regression

Before changing the normalizer, the two new production-module tests were run:

```sh
cargo test -p rustykrab-control \
  errors::fingerprint::tests::short_git_revision_ranges -- --nocapture
```

The equivalence regression failed with exit 101: actual
`diff of <id>..<n>: paths disagree`, expected
`diff of <id>..<id>: paths disagree`. The unrelated-word/shape control passed
(1 passed, 1 failed, 478 filtered out). The first failure log is retained in
the collaborating task's `normalizer-repair-red.log`.

The equivalence test covers all nine pairs of mixed, numeric-only and
letter-only revisions. It checks both normalized text and the actual
production fingerprint function. Other cases cover uppercase revisions and
punctuation, ordinary numbers, standalone words, other range lengths,
triple-dot syntax, paths, URLs, quoted values and Unicode. Existing tests
retain subclass, tool, worker-kind and wording distinctions.

After the change, all seven fingerprint-module tests passed. Formatting,
architecture facts, environment documentation and its fixture suite, and
`cargo clippy --workspace --all-targets -- -D warnings` passed. The complete
default-feature workspace suite (`cargo test --workspace -- --test-threads=1`)
passed **2,374 tests, 0 failed, 38 ignored** in the host test context.

The first sandboxed workspace attempt stopped in two unchanged agent fixtures:
one could not spawn `ps` (EPERM), and the other left its temporary Git branch
unchanged. That partial run (291 passed, 2 failed, 2 ignored) and its log are
retained. With source unchanged, those two fixtures passed individually in
the host context before the complete workspace suite passed there. No fixture
assertion was changed or skipped. The retained first workspace log has SHA-256
`f0a0da42a83e0935c518911c11bd463f5bac5c7d36f44d7185d9b9b3c4b59793`;
the complete host-context log is
`77a87bedac70d320135ba7b71b949bdca3de3c085c7b4091eb246364a02c942b`.

## Limits

- Two twelve-digit numeric values in this exact range shape also become
  identifier placeholders. Other range lengths and standalone numeric values
  are unchanged.
- Existing numeric/letter-only fingerprint history is not migrated or aliased.
- The historical CI normalizer reproduction demonstrates this defect. The
  two retained investigation-test failures on the private #701 carry did not
  print their error rows or revision ranges, so their exact causal connection
  is still unestablished. They are not erased or called repaired by this test.
- Passing repository checks is separate from a merged change, deployment and
  effective recovery of real work. This branch grants no deployment authority
  and changes no owner hold or functional-proof boundary.
