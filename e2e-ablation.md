# Context-window ablation — gemma4:26b

model max context: 262144 · compaction ceiling: 65536 · expansion target: 131072

## Speed

| window | load s | resident GB | gen t/s | prompt t/s (fixed) | prompt t/s (scaled) |
|---|---|---|---|---|---|
| 4096 | 2.7 | 17.4 | 49 | 571 | — |
| 8192 | 2.7 | 17.6 | 51 | 409 | — |
| 16384 | 2.6 | 17.6 | 51 | 499 | 517 |
| 32768 | 2.9 | 17.7 | 51 | 593 | 468 |
| 65536 | 2.9 | 18.3 | 51 | 630 | 435 |
| 131072 | 3.4 | 18.6 | 51 | 630 | 413 |
| 262144 | 0.0 | 17.6 | 52 | 572 | 397 |

## Accuracy (suite pass / applicable)

| window | pass | fail | n/a by design | mean scenario ms |
|---|---|---|---|---|
| 4096 | 17 | 2 | 0 | 132578 |
| 8192 | 18 | 1 | 0 | 82606 |
| 16384 | 16 | 0 | 3 | 143641 |
| 32768 | 16 | 0 | 3 | 143208 |
| 65536 | 16 | 0 | 3 | 79246 |
| 131072 | 16 | 0 | 3 | 112114 |
| 262144 | 16 | 0 | 3 | 155785 |

## Compaction cost (identical history; expansion off vs on)

| window | baseline ms | baseline ok | expanded ms | expanded ok |
|---|---|---|---|---|
| 4096 | 165896 | pass | 223184 | pass |
| 8192 | 893757 | pass | 95701 | pass |
| 16384 | 351272 | pass | 146623 | pass |
| 32768 | 300667 | fail | 200407 | fail |
| 65536 | 1843566 | fail | 1841760 | fail |
