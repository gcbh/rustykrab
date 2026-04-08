# Memory Architecture

This document describes how RustyKrab manages information across conversations, from persistent long-term storage through ephemeral context windowing.

## Storage Backend

All persistent data lives in a single [sled](https://docs.rs/sled) embedded database at `~/.local/share/rustykrab/db`. The `Store` struct (`crates/rustykrab-store/src/lib.rs`) opens six named trees at startup:

| Tree | Contents |
|------|----------|
| `conversations` | Full conversation message history |
| `secrets` | AES-256-GCM encrypted credentials |
| `memories` | Associative memory entries |
| `kg_entities` | Knowledge graph entities |
| `kg_relations` | Knowledge graph relationships |
| `kg_entity_names` | Name-to-UUID index for entities |

The master encryption key is wrapped in `Zeroizing<Vec<u8>>` and sourced from the macOS Data Protection Keychain or the `RUSTYKRAB_MASTER_KEY` environment variable.

## Associative Memory

**Files:** `crates/rustykrab-store/src/memory.rs`, `crates/rustykrab-tools/src/memory_save.rs`

Each memory entry stores a fact, a set of association tags, and access metadata:

```rust
MemoryEntry {
    id: Uuid,
    conversation_id: Uuid,
    fact: String,
    tags: Vec<String>,
    created_at: DateTime<Utc>,
    last_accessed: DateTime<Utc>,
    access_count: u32,
}
```

**Writing.** The agent calls the `memory_save` tool during conversation, providing both the fact and semantic tags that capture the concepts that should trigger future recall.

**Retrieval.** On every incoming user message, `build_and_inject_system_prompt()` extracts keywords (tokenization + stopword filtering, no LLM call), then calls `MemoryStore::recall()`. This matches keywords against stored tags using bidirectional substring comparison, scoped to the current conversation. Up to 10 matching memories are injected into the system prompt as a "RECALLED MEMORIES" section.

**Reconsolidation.** Each successful recall updates `last_accessed` and increments `access_count` on the matched entry, so frequently-accessed memories are ranked higher in future results.

**Deletion.** The agent can explicitly delete outdated memories via the `memory_delete` tool. There is no automatic eviction policy.

## Knowledge Graph

**File:** `crates/rustykrab-store/src/knowledge_graph.rs`

A persistent entity-relationship graph for structured information that would otherwise bloat the context window.

- **Entities** have a type (Person, Project, Event, Preference, Task, Location, Organization, Topic, or Custom), a name, and free-form JSON attributes.
- **Relations** connect two entities with a typed edge (WorksWith, DependsOn, Prefers, ScheduledFor, BelongsTo, RelatedTo, CreatedBy, AssignedTo, or Custom).
- A secondary `entity_names` tree provides case-insensitive O(1) name lookups.

Subgraph extraction uses breadth-first traversal from seed entity IDs up to a configurable hop limit, and formats the result as readable text for prompt injection via `subgraph_to_context()`.

Unlike associative memory, the knowledge graph is not automatically queried. The agent accesses it through explicit tool calls.

## Context Windowing

**File:** `crates/rustykrab-agent/src/runner.rs`

The system uses sliding-window truncation rather than LLM-based summarization to manage context length.

**Budget.** The live message budget is calculated from the harness profile:

```
live_budget = max_context_tokens * (1 - summary_budget_ratio - response_reserve_ratio)
```

With defaults (128K context, 20% summary reserve, 15% response reserve), this gives ~83K tokens for messages.

**Trigger.** Truncation fires when estimated token usage exceeds 85% of the live budget.

**Mechanism.** Starting from the most recent message, the system walks backward accumulating token estimates (~3.5 chars/token) until it reaches 60% of the budget. Everything before that point is dropped, except the system message at index 0, which is always preserved.

The agent is responsible for saving important information via `memory_save` before it scrolls out of context.

## Recursive Context Budgeting

**File:** `crates/rustykrab-agent/src/rlm/context_manager.rs`

When the orchestration pipeline decomposes a task into sub-tasks, each recursive call gets a shrinking context budget:

- Each depth level receives 75% of the parent's budget
- Floor of 2,048 tokens (below which model output quality degrades)
- Hard cutoff at `max_recursion_depth` (default 3)
- Default sub-task budget: 16,384 tokens

## Execution Tracing

**File:** `crates/rustykrab-agent/src/trace.rs`

Each agent run gets a fresh `ExecutionTracer` (per-session isolation prevents cross-session data leaks). It records:

- Per-tool-call outcomes: name, success/failure, duration, error message
- Aggregated stats: call count, success rate, average duration per tool
- Iteration and compression counters

Every 5 iterations, a trace summary is injected into the conversation identifying unreliable tools (>50% failure rate over 2+ calls) and suggesting alternative approaches. Tool names are sanitized before recording to prevent prompt injection.

## Harness Profiles

**File:** `crates/rustykrab-agent/src/harness.rs`

All memory-related parameters are bundled into `HarnessProfile`, a serializable configuration object that can be swapped at runtime based on task type:

| Parameter | General | Coding | Research |
|-----------|---------|--------|----------|
| `max_iterations` | 80 | 120 | 80 |
| `max_context_tokens` | 128K | 128K | 128K |
| `summary_budget_ratio` | 0.20 | 0.20 | 0.25 |
| `response_reserve_ratio` | 0.15 | 0.20 | 0.15 |
| `trace_injection_interval` | 5 | 5 | 5 |

The model self-classifies each response with a profile tag, allowing the harness to adapt as the conversation evolves.

## Data Flow Summary

```
User message arrives
  │
  ├─ extract_keywords(message)
  │    └─ MemoryStore.recall(conv_id, keywords)
  │         └─ inject top 10 matches into system prompt
  │
  ├─ Build system prompt (identity, tools, CoT, security policy, skills)
  │
  └─ Agent loop begins
       │
       ├─ Check context budget → truncate_oldest() if over 85%
       ├─ LLM call with conversation + tool schemas
       ├─ Execute tool calls (parallel, sandboxed, traced)
       │    ├─ memory_save   → MemoryStore.save()
       │    ├─ memory_search → MemoryStore.recall()
       │    ├─ memory_delete → MemoryStore.delete()
       │    └─ (knowledge graph tools → KnowledgeGraph.*)
       ├─ Record traces → ExecutionTracer
       ├─ Every 5 iterations: inject trace guidance
       └─ On repeated errors: inject reflection prompt
```
