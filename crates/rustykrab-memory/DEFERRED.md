# Deferred Work — rustykrab-memory

Tracks what the memory crate implements today versus what remains to be built. Items are ordered by impact.

## High impact

### Real embedding model (fastembed)

The `Embedder` trait is implemented and pluggable. Currently only test embedders (`HashEmbedder`, `ZeroEmbedder`) exist. Integrating `fastembed` v5.x (wraps ONNX Runtime) would provide real vector search with models like Nomic-embed-text-v1.5 (768d, 8K context, Apache 2.0) or BGE-M3 (1024d, dense+sparse+multi-vector).

**What to do:** Add `fastembed` as an optional dependency behind a feature flag. Implement `Embedder` for it. Use `tokio::task::spawn_blocking` for CPU-bound inference (~15–25ms per query on CPU).

### Gateway wiring

`HybridMemoryBackend` in `backend.rs` exposes the same `search`/`get`/`save`/`delete`/`list` API that `MemoryBackend` in `rustykrab-tools` expects. It needs to be wired into `AppState` in `rustykrab-gateway` and passed to `memory_tools()` in `rustykrab-tools/src/lib.rs` so the agent actually uses hybrid retrieval.

**What to do:** In `rustykrab-gateway/src/state.rs`, construct a `MemorySystem` with `SqliteMemoryStorage` and wrap it in `HybridMemoryBackend`. Pass that as the `MemoryBackend` to `memory_tools()`. Call `system.rebuild_indexes(agent_id)` on startup to hydrate the BM25 index.

### Integration tests

Unit tests cover individual modules (BM25, chunking, embedding, extraction, scoring). There are no end-to-end tests that exercise the full write → retrieve → lifecycle pipeline through `MemorySystem`.

**What to do:** Add tests in `tests/integration.rs` using `SqliteMemoryStorage::open_in_memory()` and `HashEmbedder`. Test: retain a turn, recall it, verify RRF sources, run lifecycle sweep, verify promotion/demotion, verify dedup skips exact duplicates.

## Medium impact

### Cross-encoder reranking

The retrieval pipeline currently skips Stage 4 (reranking) from the blueprint. After RRF fusion, candidates go straight to lifecycle scoring. Adding a cross-encoder reranker (gte-reranker-modernbert-base at 149M params, or ms-marco-MiniLM-L-6-v2 for speed) would improve precision on the top-K.

**What to do:** Define a `Reranker` trait with `rerank(query: &str, candidates: Vec<(Uuid, String)>, limit: usize) -> Vec<(Uuid, f64)>`. Integrate into `retrieval.rs` between RRF fusion and lifecycle scoring. Implement via `fastembed`'s `BGERerankerBase` behind a feature flag.

### FTS5 for in-database keyword search

The BM25 index is currently in-memory (rebuilt on startup from persisted memories). SQLite's FTS5 extension could replace it entirely, moving keyword search into SQL and eliminating the need to hold the inverted index in memory.

**What to do:** Add a `memories_fts` virtual table: `CREATE VIRTUAL TABLE memories_fts USING fts5(content, content_rowid='rowid')`. Populate on insert. Replace the BM25 retrieval arm in `retrieval.rs` with a SQL query against `memories_fts`.

### LLM-scored importance

The heuristic importance scorer (`scoring.rs`) runs synchronously at write time. The blueprint calls for an async background pass using a cheap LLM (GPT-4o-mini, Qwen2.5-1.5B) with the Park et al. prompt: *"On a scale of 1–10, rate this memory's poignancy."*

**What to do:** After `writer.rs` stores the memory, spawn an async task that calls the LLM, then updates the `importance` column and sets `importance_source = 'llm'`. Use the existing `ModelProvider` trait from `rustykrab-core`.

### HNSW vector index

Brute-force cosine similarity scans all chunk embeddings on every query. This is fine up to ~100K chunks but becomes a bottleneck at scale. The blueprint recommends `usearch` (single-file HNSW, SIMD-accelerated) or `sqlite-vec` (vector search as a SQLite extension).

**What to do:** Add `usearch` as an optional dependency. Build the HNSW index on startup from stored embeddings. Update `retrieval.rs` to use it instead of `embedding::top_k_similar`. Alternatively, integrate `sqlite-vec` to keep everything in a single SQLite file.

## Lower impact

### Consolidation pipeline

The lifecycle manager handles stage transitions (promote/demote/tombstone) but does not merge or synthesize memory content. The blueprint describes an after-session batch job that: (1) detects near-duplicates (implemented), (2) merges them into consolidated memories, (3) resolves contradictions with temporal narratives, (4) promotes episodic facts to semantic knowledge.

**What to do:** Steps 2–4 require LLM calls. Add a `consolidate()` method to `LifecycleManager` that takes near-duplicate clusters (already detected), calls an LLM to synthesize, creates new consolidated memories with `parent_memory_ids` linking to sources, and invalidates the originals.

### Learned importance model

The blueprint mentions ACAN (cross-attention network) as a future optimization: train a small model on LLM-scored importance data (435+ examples suffice) to predict importance without LLM calls. Only worth pursuing after accumulating labeled data from the LLM-scored importance step above.

### Embedding drift alerting

`LifecycleManager::check_embedding_drift()` is implemented — it re-embeds a sample and compares cosine distance. What's missing is: (1) a periodic scheduler to run it (e.g., weekly via the automation/cron system), (2) an alerting mechanism when drift exceeds the 0.05 threshold, (3) a full re-index workflow that atomically re-embeds all chunks from the SQLite source of truth.

### Memory inspection / correction UI

The blueprint warns about error propagation: bad retrievals produce bad outputs that get stored as new bad memories. Users need tools to inspect, correct, and delete specific memories. The `memory_get`, `memory_delete` tools exist but a richer inspection interface (list by stage, search by date range, view linked memories) would help.
