# Context-window ablation — gemma4:26b

model max context: 262144 · compaction ceiling: 65536 · expansion target: 131072

## Speed

| window | load s | resident GB | gen t/s | prompt t/s (fixed) | prompt t/s (scaled) |
|---|---|---|---|---|---|
| 8192 | 0.0 | 0.0 | 0 | 0 | — |

## Accuracy (suite pass / applicable)

| window | pass | fail | n/a by design | mean scenario ms |
|---|---|---|---|---|
| 8192 | — window did not serve — ||||

## Compaction cost (identical history; expansion off vs on)

| window | baseline ms | baseline ok | expanded ms | expanded ok |
|---|---|---|---|---|
