# Context-window ablation — gemma4:26b

model max context: 262144 · compaction ceiling: 65536 · expansion target: 131072

## Speed

| window | load s | resident GB | gen t/s | prompt t/s (fixed) | prompt t/s (scaled) |
|---|---|---|---|---|---|
| 8192 | 28.5 | 17.6 | 51 | 629 | — |

## Accuracy (suite pass / applicable)

| window | pass | fail | n/a by design | mean scenario ms |
|---|---|---|---|---|
| 8192 | 1 | 0 | 0 | 12691 |

## Compaction cost (identical history; expansion off vs on)

| window | baseline ms | baseline ok | expanded ms | expanded ok |
|---|---|---|---|---|
| 8192 | 492052 | pass | 758651 | pass |
