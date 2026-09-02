# rustykrab-memory — Hybrid Retrieval

13 files, ~5,840 lines, 68 tests + a lifecycle integration harness. Depends
only on `rustykrab-core`. **Zero `env::var` reads** — the most portable crate
in the workspace.

## Responsibility

Own `memory.db` and everything about agent memory: verbatim storage, four-way
parallel retrieval with rank fusion, and a value-driven lifecycle that bounds
the working set.

## Module map

| Module | Lines | Role |
|---|---|---|
| `storage.rs` | 1,898 | `MemoryStorage` trait + SQLite implementation, schema, FTS5 |
| `writer.rs` | 467 | Write path: admission → chunk → embed → persist → link |
| `embedding.rs` | 440 | `Embedder` trait; `HashEmbedder`, `LazyFastEmbedder` |
| `backend.rs` | 437 | `HybridMemoryBackend` — the tool-facing surface |
| `lifecycle.rs` | 379 | Decay, promotion, demotion, sweep |
| `retrieval.rs` | 300 | Four-arm parallel recall + RRF fusion |
| `scoring.rs` | 293 | Weighted Reciprocal Rank Fusion (k=60) |
| `lib.rs` | 295 | `MemorySystem` façade |
| `types.rs` | 243 | `ConversationTurn`, `RetrievalResult`, `ExtractedFact`, … |
| `admission.rs` | 263 | Content gate with per-cause rejection reasons |
| `extraction.rs` | 218 | Async fact extraction (subject/predicate/object) |
| `chunking.rs` | 183 | Chunk splitting |
| `config.rs` | 125 | `MemoryConfig` — one struct, passed in, not read from env |

## Design

Three stated pillars, all visible in the code:

1. **Verbatim first.** Raw text is stored synchronously as source of truth;
   fact extraction is async and additive. Retrieval never depends on extraction
   having succeeded.
2. **Four-arm retrieval.** Semantic (cosine over chunk embeddings), keyword
   (FTS5), graph (link expansion), temporal (recency) run concurrently under
   `tokio::join!`, then fuse with weighted RRF. Results carry provenance
   (`RetrievalSource`), so the agent can see *why* something surfaced.
3. **Value-driven lifecycle.** `working → episodic → semantic` on promotion,
   `→ archival → tombstone` on demotion, with importance-modulated exponential
   decay. Soft delete (`is_valid`), never hard delete.

`recall_filtered` applies the session filter *before* recording access, with the
reasoning stated in the code: bumping access counts on memories excluded by the
filter would grant every scoped search a phantom relevance boost to other
conversations' memories. That is the kind of second-order correctness thinking
that distinguishes this crate.

## Why this crate is the reusability benchmark

- One `MemoryConfig` struct, constructed by the caller. No ambient state.
- Storage behind a trait, with an in-memory constructor for tests.
- Embedding behind a trait, with a deterministic `HashEmbedder` so the crate
  builds and tests without downloading an ONNX runtime.
- A doc-comment quick-start that actually compiles as written.

Every other crate's portability problems are the absence of one of these four.

## Observations

- **`session_id` now consistently means the conversation.** `memory_save`
  takes it from the ambient tool context, the same source `search` already
  used to record retrievals; the construction-time id is named
  `fallback_scope` and is used only outside a runner. Shutdown finalizes the
  agent's whole working set rather than a process-boot session that was
  always empty. The symptom this fixed was subtle — a global fallback meant
  saved facts were reachable, just never *scoped*, so every scoped search
  quietly widened.
- **Semantic search is a linear scan.** `get_all_chunk_embeddings` pulls every
  non-null embedding for the agent into process memory (via a correct join,
  cached per agent, invalidated on write) and cosine-scores in Rust. Right at
  thousands of memories, wrong at hundreds of thousands. The design should state
  which regime it targets — the lifecycle machinery exists precisely to keep the
  working set small, so the answer is probably "bounded by design", and that
  should be written down.
- **`memory_links` has no foreign keys** while `chunks` and `extracted_facts`
  do. The asymmetry looks accidental.
- **`HybridMemoryBackend` implements `MemoryBackend` directly** now that the
  trait lives in `rustykrab-core`; the binary's pass-through adapter is gone,
  and the decision about what an unparseable conversation id means moved here,
  where the cost of widening is actually known.
- **Config is validated at construction.** `try_new` returns the error, `new`
  panics with a clear message. `rrf_k == 0.0` and `chunk_max_tokens == 0` used
  to surface as a panic on the first query, arbitrarily far from the code that
  chose the value.
- **A poisoned `FastEmbedder` lock no longer disables memory permanently.**
  ONNX inference can abort the thread; the embedder is shared, so one bad
  batch used to return "embedder lock: poisoned" until restart.
- `storage.rs` at 1,898 lines mixes schema, trait, and per-entity query blocks;
  it splits cleanly along entity lines.
