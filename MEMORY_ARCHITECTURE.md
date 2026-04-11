# Memory Architecture

This document describes how RustyKrab manages information across conversations, from session-scoped working memory through long-term semantic knowledge.

## Overview

The memory system is implemented in the `rustykrab-memory` crate and uses a three-pillar architecture:

1. **Verbatim storage** -- raw conversation text is stored synchronously as the source of truth
2. **Four-way parallel retrieval** -- semantic, keyword, graph, and temporal strategies run concurrently and fuse with Reciprocal Rank Fusion
3. **Value-driven lifecycle** -- importance-modulated exponential decay manages the hot retrieval set

All persistent data lives in a SQLite database at `~/.local/share/rustykrab/memory.db`, using WAL mode for concurrent reads and crash safety.

## Storage Schema

**Files:** `crates/rustykrab-memory/src/storage.rs`

| Table | Purpose |
|-------|---------|
| `memories` | Core memory records with lifecycle metadata, scoring, and access patterns |
| `chunks` | Embedding chunks for vector retrieval (one memory = one or more chunks) |
| `extracted_facts` | Subject-predicate-object triples extracted from memory content |
| `memory_links` | Graph edges between memories (semantic similarity, co-occurrence, causal chains) |

The `memories` table stores each record with: content, SHA-256 content hash (for exact dedup), lifecycle stage, importance score, decay rate, access count, timestamps, consolidation metadata, tags, and free-form JSON metadata (including `session_id`).

## Memory Lifecycle

**Files:** `crates/rustykrab-memory/src/types.rs`, `crates/rustykrab-memory/src/lifecycle.rs`

Memories progress through five stages based on access patterns and value:

```
Working (active session)
  |
  v  [finalize_session / sweep idle timeout]
Episodic (past conversations)
  |
  +---> Semantic  (promoted: accessed >= 3x, age >= 7 days)
  |
  +---> Archival  (demoted: effective score < 0.05, idle > 30 days)
          |
          +---> Tombstone (idle > 180 days, importance < 0.3)
```

### Working

Active working memory for the current session. Created when a conversation turn is retained via `MemoryWriter::retain()`. Working memories are included in the hot retrieval set so the agent can recall them during the active session.

**Transition to Episodic:**
- **Primary:** `MemorySystem::finalize_session(agent_id, session_id)` -- called when a session ends, batch-promotes all Working memories for that session to Episodic.
- **Safety net:** The lifecycle sweep auto-promotes stale Working memories (idle > `working_max_idle_minutes`, default 60 minutes) to catch orphaned sessions from crashes or missed finalizations.

### Episodic

Recent memories from past conversations. The main stage for active recall. Subject to two transitions:

- **Promotion to Semantic:** Accessed >= `promote_min_access_count` (default 3) times AND older than `promote_min_age_days` (default 7) days.
- **Demotion to Archival:** Effective score falls below `archive_score_threshold` (default 0.05) AND idle > 30 days.

### Semantic

Consolidated long-term knowledge. Promoted from Episodic based on frequent access and age. Not subject to further automatic transitions.

### Archival

Low-value memories moved out of the hot retrieval set. Excluded from `recall()` results but retained in storage.

- **Transition to Tombstone:** Idle > `tombstone_idle_days` (default 180) AND importance < `tombstone_importance_threshold` (default 0.3).

### Tombstone

Soft-deleted. Excluded from all retrieval and lifecycle sweeps. Retained for audit purposes.

## Write Path

**File:** `crates/rustykrab-memory/src/writer.rs`

The write path uses a dual-track design:

### Track 1 (Synchronous)

1. **SHA-256 dedup** -- if identical content already exists, skip write and bump access count on the existing memory
2. **Store verbatim** -- insert the memory record as `Working` stage with heuristic importance score
3. **Chunk + embed** -- split content into overlapping chunks (512 tokens, 15% overlap), embed each chunk using the configured model (Nomic-embed-text-v1.5, 768 dimensions)
4. **BM25 index** -- add the content to the in-memory BM25 inverted index

### Track 2 (Asynchronous, never blocks)

5. **Fact extraction** -- regex-based extraction of preferences, decisions, key-value pairs, and entities
6. **Near-duplicate detection** -- compare the new memory's embedding against existing memories; if cosine similarity >= 0.95 (`dedup_auto_merge_threshold`), invalidate the new memory and bump access on the existing one

## Retrieval Pipeline

**File:** `crates/rustykrab-memory/src/retrieval.rs`

Four retrieval arms run in parallel via `tokio::join!`:

| Arm | Strategy | Weight |
|-----|----------|--------|
| Semantic | Cosine similarity over chunk embeddings | 1.0 |
| Keyword | BM25 search over in-memory inverted index | 1.0 |
| Graph | Seed top-5 semantically similar memories, expand 1-hop via precomputed links | 0.8 |
| Temporal | Most recent memories within a 30-day sliding window | 0.6 |

Results are fused using weighted Reciprocal Rank Fusion (k=60), then multiplied by each memory's `effective_score()` which combines importance, temporal decay, and access boost:

```
effective_score = importance * e^(-effective_decay * idle_hours / 168) * access_boost * query_similarity
```

where `effective_decay = decay_rate * (1 - importance * 0.8)` so high-importance memories decay up to 5x slower.

Only memories in the hot retrieval set (Working, Episodic, Semantic) are returned.

## Lifecycle Sweep

**File:** `crates/rustykrab-memory/src/lifecycle.rs`

The lifecycle sweep runs as a background job, triggered by:
- **Session end:** after `finalize_session()` during shutdown
- **Idle breaks:** when no activity for `sweep_idle_trigger_minutes` (default 5 minutes)

Each sweep executes four steps in order:

1. **Working -> Episodic** -- promote stale Working memories (idle > `working_max_idle_minutes`)
2. **Episodic -> Semantic** -- promote frequently accessed, aged memories
3. **Episodic -> Archival** -- demote low-scoring, idle memories
4. **Archival -> Tombstone** -- tombstone old, low-importance archival memories

The sweep also supports near-duplicate detection via `detect_near_duplicates()`, which creates bidirectional `SemanticSimilar` links between memories with cosine similarity >= 0.85.

## Importance Scoring

**File:** `crates/rustykrab-memory/src/scoring.rs`

Importance is scored heuristically at write time on a [0.0, 1.0] scale:

| Factor | Boost |
|--------|-------|
| Baseline | 0.30 |
| Named entity (per entity, max 5) | +0.05 |
| Temporal marker (dates, times) | +0.05 |
| Tool usage | +0.15 |
| User-flagged | +0.30 |

Future: LLM-scored importance via async background pass (see `DEFERRED.md`).

## Deduplication

Two layers prevent storing redundant memories:

1. **Exact dedup (synchronous):** SHA-256 content hash check at write time. Identical content is never stored twice.
2. **Near-duplicate detection (asynchronous):** After storing, the background task compares the new memory's embedding against existing memories. Cosine similarity >= 0.95 triggers auto-invalidation of the new memory, with an access bump on the existing one.

## Session Model

Each agent run creates a new `session_id` (UUID). Memories track their session via `metadata.session_id`. When a session ends:

1. `finalize_session(agent_id, session_id)` promotes all Working memories from that session to Episodic
2. A lifecycle sweep runs to cascade further transitions

When the user returns, a new session starts. Episodic memories from past sessions are discoverable via the retrieval pipeline -- no re-promotion to Working occurs.

Conversation history (raw message logs in the `conversations` sled tree) is always accessible via API regardless of memory lifecycle stage.

## Configuration

**File:** `crates/rustykrab-memory/src/config.rs`

All tuning parameters are bundled in `MemoryConfig`:

| Category | Parameter | Default | Description |
|----------|-----------|---------|-------------|
| Chunking | `chunk_max_tokens` | 512 | Max tokens per chunk |
| Chunking | `chunk_overlap_ratio` | 0.15 | Overlap between chunks |
| Retrieval | `retrieval_candidates_per_arm` | 50 | Over-fetch candidates per arm |
| Retrieval | `rrf_k` | 60.0 | RRF fusion constant |
| Retrieval | `rrf_weight_semantic` | 1.0 | Semantic arm weight |
| Retrieval | `rrf_weight_keyword` | 1.0 | Keyword arm weight |
| Retrieval | `rrf_weight_graph` | 0.8 | Graph arm weight |
| Retrieval | `rrf_weight_temporal` | 0.6 | Temporal arm weight |
| Retrieval | `default_recall_limit` | 10 | Default results to return |
| Lifecycle | `default_decay_rate` | 1.0 | Decay rate (37% after 1 idle week) |
| Lifecycle | `default_importance` | 0.5 | Default importance for new memories |
| Lifecycle | `archive_score_threshold` | 0.05 | Score below which Episodic -> Archival |
| Lifecycle | `promote_min_access_count` | 3 | Min accesses for Episodic -> Semantic |
| Lifecycle | `promote_min_age_days` | 7 | Min age for Episodic -> Semantic |
| Lifecycle | `tombstone_idle_days` | 180 | Idle days for Archival -> Tombstone |
| Lifecycle | `tombstone_importance_threshold` | 0.3 | Importance below which Archival -> Tombstone |
| Lifecycle | `working_max_idle_minutes` | 60 | Idle minutes before Working -> Episodic (sweep) |
| Lifecycle | `sweep_idle_trigger_minutes` | 5 | Idle minutes before running a sweep |
| Dedup | `dedup_auto_merge_threshold` | 0.95 | Cosine threshold for near-duplicate invalidation |
| Dedup | `dedup_distinct_threshold` | 0.85 | Cosine threshold for similarity linking |
| Embedding | `embedding_dimensions` | 768 | Vector dimensionality |
| Embedding | `embedding_model_version` | nomic-embed-text-v1.5 | Embedding model identifier |

## Context Windowing

**File:** `crates/rustykrab-agent/src/runner.rs`

The system uses sliding-window truncation to manage context length. The live message budget is:

```
live_budget = max_context_tokens * (1 - summary_budget_ratio - response_reserve_ratio)
```

With defaults (128K context, 20% summary reserve, 15% response reserve), this gives ~83K tokens. Truncation fires at 85% of the live budget, keeping the system message and walking backward from the most recent message to preserve 60% of the budget.

The agent is responsible for saving important information via `memory_save` before it scrolls out of context.

## Execution Tracing

**File:** `crates/rustykrab-agent/src/trace.rs`

Each agent run gets a per-session `ExecutionTracer` that records per-tool-call outcomes and aggregated stats. Every 5 iterations, a trace summary identifies unreliable tools (>50% failure rate) and suggests alternative approaches.

## Tool Interface

**File:** `crates/rustykrab-tools/src/memory_*.rs`

The agent interacts with memory through four explicit tool calls:

| Tool | Description |
|------|-------------|
| `memory_save` | Save a fact with association tags |
| `memory_search` | Query-time retrieval (backed by hybrid pipeline) |
| `memory_get` | Fetch a specific memory by ID |
| `memory_delete` | Soft-delete (invalidate) a memory |

Memory is NOT automatically injected into the system prompt. The agent decides when to save and search.

## Data Flow Summary

```
User message arrives
  |
  +-- Agent loop begins
  |     |
  |     +-- Check context budget -> truncate if over 85%
  |     +-- LLM call with conversation + tool schemas
  |     +-- Execute tool calls (parallel, sandboxed, traced)
  |     |     +-- memory_save  -> MemoryWriter::retain() [Working stage]
  |     |     +-- memory_search -> MemoryRetriever::recall() [4-way parallel + RRF]
  |     |     +-- memory_get   -> MemoryStorage::get_memory()
  |     |     +-- memory_delete -> LifecycleManager::invalidate_memory()
  |     +-- Record traces -> ExecutionTracer
  |     +-- Every 5 iterations: inject trace guidance
  |     +-- On repeated errors: inject reflection prompt
  |
  +-- Session ends
        |
        +-- finalize_session() [Working -> Episodic]
        +-- lifecycle_sweep() [promote/demote/tombstone]
        +-- flush database

Idle breaks (no activity for 5 min)
  |
  +-- lifecycle_sweep() [promote stale Working, demote/tombstone]
```
