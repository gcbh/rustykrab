# Retained evaluation evidence

Raw experiment output is intentionally excluded from the replacement PR diffs.
The original source and evidence remain pinned to commit
`5790e24d7b7acc900f94844f672e96c2dd0cd938` on the preserved
`codex/broadway-context-eval` branch. Do not delete that evidence branch.

[Original study and limitations](https://github.com/gcbh/rustykrab/blob/5790e24d7b7acc900f94844f672e96c2dd0cd938/docs/evals/compaction-study-validation.md)

[All raw captures and independent audits](https://github.com/gcbh/rustykrab/tree/5790e24d7b7acc900f94844f672e96c2dd0cd938/docs/evals/evidence)

| Bundle | SHA-256 |
| --- | --- |
| compaction-primary-raw.tar.gz | 3407120d2dd811d1b5e953a875c26c99ee752fe85322e077fa6a13f170b24cab |
| compaction-message-tail-raw.tar.gz | 1521da78dc83b74a069d89a5188ec2e67bcc8fb7b315262bc22df4e5d7f2c1fe |
| compaction-fieldwise-raw.tar.gz | 6a3f40d66127b341a1d81a6320fb4e841c4f27afa5f34766fcd7731a4687dc4a |
| runtime-validation-raw.tar.gz | 82cd26c6fe96b391902e74742048e9ce6f587725e202933875b6116071d1213c |

The bundles contain synthetic histories, request captures, visible readouts,
unchanged original grades and validation logs. Original experiment manifests
identify the tested builds; those runs are not relabelled as new split-branch
experiments. New checks validate each split separately. Existing local copies
and the original PR branch are retained, not erased.
