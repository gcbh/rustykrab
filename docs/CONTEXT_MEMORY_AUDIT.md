# Context & Memory System Audit

**Date:** 2026-07-22
**Scope:** `rustykrab-agent` (context windowing/compaction), `rustykrab-memory` (long-term memory), `rustykrab-store` (conversation persistence), `rustykrab-gateway`/`rustykrab-cli` (channel wiring)
**Trigger:** User report — "specific chats are having context get dropped or not remembered."

---

## 1. Executive summary

The report of *specific* chats losing context is explained by real, confirmed defects — not tuning issues. The most important finding is that **the two agent entry points are wired differently**: the HTTP/WebChat/Slack/scheduled-task paths get memory persistence and a durable recall archive, while the **Telegram (interactive) path gets neither**. On top of that, a **token-estimation bug makes any conversation containing an image appear hundreds of thousands of tokens large**, forcing destructive compaction on every iteration, and **compaction itself discards the newest user messages**, keeping only the oldest.

The long-term memory system has strong bones (verbatim store + hybrid retrieval + lifecycle) but several of its defaults and failure modes silently destroy or hide memories: month-idle memories fall below the archive threshold, updated facts are auto-invalidated in favor of their stale predecessors, an embedder failure blacks out both writes and reads, and — most fundamentally — **nothing ever tells the model to use the memory tools**, so the read side of memory is almost never exercised.

Severity legend: **P0** = directly causes user-visible context loss today · **P1** = causes loss in common scenarios · **P2** = degrades quality/robustness · **P3** = hygiene.

| # | Sev | Area | Finding |
|---|-----|------|---------|
| 1 | P0 | Compaction | Image bytes counted as text: ~285k "tokens" for a 1 MB photo → perpetual forced compaction |
| 2 | P0 | Compaction | Compaction keeps only system prompt + *first* user message; the *newest* messages are summarized away |
| 3 | P0 | Channels | Telegram path wires no memory callback and no durable recall store — chats on that channel are never remembered |
| 4 | P0 | Channels | Telegram stall/timeout/cancel path never persists the conversation — the whole turn vanishes |
| 5 | P1 | Memory | Near-duplicate detection invalidates the *new* memory, preserving the stale fact |
| 6 | P1 | Memory | Decay math archives default-importance memories after ~30 idle days — recall goes dark on them |
| 7 | P1 | Memory | Embedder failure = memory blackout (writes half-indexed and invisible; recall errors outright) |
| 8 | P1 | Prompting | System prompt contains zero memory guidance — model never told to search or save |
| 9 | P1 | Compaction | 64 KiB compaction ceiling wastes 70%+ of a 200k model's window |
| 10 | P1 | Sessions | `finalize_session` called with a session ID that matches no memories |
| 11 | P2 | Channels | Telegram media without caption silently dropped; images never reach the model |
| 12 | P2 | Persistence | Conversation saved only after a run completes; whole-blob JSON rewrite; no incremental append |
| 13 | P2 | Compaction | Compaction failure aborts the whole run |
| 14 | P2 | Memory | Auto-persisted tool-call/tool-result turns pollute memory verbatim |
| 15 | P2 | Retrieval | Relevance score multiplied by decay/importance conflates "relevant" with "recent" |
| 16 | P3 | Memory | Graph arm mostly re-returns near-duplicates; co-occurrence/causal links unimplemented |
| 17 | P3 | Scale | Brute-force cosine over all chunk embeddings per recall |
| 18 | P3 | Docs | MEMORY_ARCHITECTURE.md materially out of date (claims truncation, in-memory BM25, sled tree) |

---

## 2. How the system actually works today

**Per-message flow (Telegram example).** A chat/thread maps to a persistent `conversation_id` via `chat_map` (SQLite), the full conversation JSON blob is loaded, the new user message is appended, and `run_agent_interactive` starts an `AgentRunner` loop. Each iteration checks `needs_compaction` (estimated tokens ≥ 85% of `min(provider_limit, 64k ceiling)`); if it fires, the LLM writes a ≤8k-token summary, displaced messages are archived to a recall store, and the in-context history is replaced by `[system, first user message, summary, "continue" prompt]`. On clean completion the final conversation blob is saved back.

**Memory flow.** On the HTTP/streaming paths, an `on_message` callback auto-retains every non-system turn into working memory (SHA-256 dedup → verbatim SQLite row → chunk+embed with fastembed/nomic → FTS5 index → async regex fact extraction + cosine near-dup check). Retrieval is 4-arm (vector, FTS5, 1-hop graph links, 30-day recency) fused with weighted RRF, multiplied by a lifecycle `effective_score` (importance × exponential idle decay × access boost). A lifecycle sweep promotes Working→Episodic→Semantic and demotes to Archival→Tombstone.

This is a genuinely decent 2024-era design (hybrid retrieval + RRF + lifecycle staging). The failures are in the wiring, the defaults, and the compaction policy — detailed below.

---

## 3. Findings

### 3.1 P0 — Direct causes of "context dropped"

#### F1. Image bytes counted as prompt text — forced compaction loops
`crates/rustykrab-agent/src/runner.rs:2281` counts an image block's size as `data.len()` (raw bytes, `Vec<u8>` per `rustykrab-core/src/types.rs:44`), then divides by 3.5 at `runner.rs:2287`:

```rust
rustykrab_core::types::ContentBlock::Image { data, .. } => data.len(),
...
(content_chars as f64 / 3.5).ceil() as usize + 4
```

A 1 MB photo is estimated at ~285,000 tokens. Real cost on Claude is ≤ ~1,600 tokens per image. Since the compaction threshold is ~55,700 tokens (85% × 64k ceiling), **one image permanently pins the conversation above the threshold**: compaction fires at `runner.rs:1242` on *every* iteration, each pass re-summarizing the summary (the image itself renders as `[image:media_type]` in the summarizer input, so its content contributes nothing). Every message in an image-bearing WebChat/API conversation triggers a full history rewrite. This is the single strongest match for "specific chats drop context."

**Fix:** estimate images at a flat ~1,500 tokens (or Anthropic's `(w×h)/750` if dimensions are known), never at byte length. Same for audio (currently counted as 0 via the `_ => 0` arm — an underestimate in the other direction).

#### F2. Compaction discards the newest messages, keeps the oldest
`runner.rs:2767–2784`: survivors are leading system messages plus **the first user message**; everything else — including the message the user just sent — exists only in the ≤8k summary:

```rust
for msg in &conv.messages {
    if msg.role == Role::System { ... new_messages.push(msg.clone()); } else { break; }
}
if let Some(first_user) = conv.messages.iter().find(|m| m.role == Role::User) { ... }
```

Every serious compactor (Claude Code, the Anthropic compaction API, OpenAI session summarization) summarizes the **old prefix** and keeps the **recent tail verbatim**, because the tail carries the active task. Here, a user who triggers compaction mid-conversation gets a model that has literally never seen their latest message — only a bullet-point paraphrase. Combined with F1, chats degrade into summary-of-summary chains.

**Fix:** keep the most recent N turns (or ~30–50% of budget walking backward from the tail, respecting tool_use/tool_result pairing) verbatim; summarize only the displaced prefix; never displace the in-flight user message.

#### F3. Telegram runs with no memory and an ephemeral recall store
`run_agent_interactive` (`crates/rustykrab-gateway/src/orchestrate.rs:369–416`) — used only by Telegram (`main.rs:1541`) — builds the runner **without** `build_memory_callback`, **without** `.with_recall_store(state.recall)`, and **without** `.with_active_tools(...)`. The other entry points (`prepare_agent`, `orchestrate.rs:287–308`, used by HTTP routes, Slack, WebChat streaming, and scheduled tasks) wire all three.

Consequences for Telegram chats specifically:
- **No conversation turn is ever written to long-term memory** — `memory_search` cannot find anything said on Telegram unless the model spontaneously called `memory_save`.
- The runner falls back to `recall: Arc::new(RecallStore::new())` (`runner.rs:818`) — a fresh **in-memory** store per run, while `AppState.recall` is backed by the durable `RecallArchiveStore` (`state.rs:83`). Compaction-displaced history is archived into a store that is dropped when the message finishes processing. The `recall_*` tools the summary explicitly advertises ("Use recall_info / recall_search … to fetch specifics", `runner.rs:2814–2818`) point at an empty store on the very next message.

This is a textbook "specific chats" bug: WebChat/Slack remember; Telegram doesn't.

**Fix:** route all channels through one preparation function (`prepare_agent`) so wiring can't diverge; add a regression test asserting the interactive runner has memory + durable recall.

#### F4. Telegram stall/timeout/cancel loses the entire turn
`main.rs:1581–1608`: the conversation is persisted **only** on the clean-join path. On heartbeat timeout the run is cancelled and the function returns without saving; the user's message, any partial assistant work, and any compaction that already archived history are all discarded (and since the recall store is ephemeral there, the archived history is unrecoverable). Slack's equivalent (`main.rs:1946`) saves unconditionally — another cross-channel inconsistency. A process crash mid-run loses the turn on every channel because saving is end-of-run only (F12).

**Fix:** persist the conversation on all exit paths (`finally`-style), and ideally append-persist each message as it is pushed (see rebuild plan).

### 3.2 P1 — Memory that forgets

#### F5. Near-dup detection keeps the stale fact and kills the update
`crates/rustykrab-memory/src/writer.rs:216–231`: after storing a new memory, a background task compares its first chunk embedding against **all** existing chunks; at cosine ≥ 0.95 it **invalidates the new memory** and bumps access on the old one:

```rust
if sim >= dedup_threshold {
    let _ = storage.invalidate(memory_id, Some(*existing_id)).await;
    let _ = storage.record_access(*existing_id).await;
}
```

"My favorite color is red" vs. "my favorite color is blue", an updated address, a changed deadline — sentence pairs like these routinely embed above 0.95. The system discards the *correction* and *reinforces* the outdated fact (the access bump slows its decay). There is no notion of fact validity intervals or supersession. This is the precise mechanism behind "I told it X and it remembers the old Y."

**Fix (minimum):** when near-dup fires, keep the **newer** memory and invalidate the older one (`invalidated_by = new_id`), or link them and let recency break ties. **Fix (proper):** temporal fact model — see §5.

#### F6. Default decay archives memories after ~30 idle days
`types.rs:120–133` with defaults (importance 0.5, decay_rate 1.0): `effective_decay = 1.0 × (1 − 0.5×0.8) = 0.6`, so after 30 idle days `score = 0.5 × e^(−0.6×720/168) ≈ 0.038` — below `archive_score_threshold = 0.05`. The sweep (`lifecycle.rs:115`) then demotes it to Archival, which `is_retrievable()` excludes from **all** recall. Anything the user hasn't touched in a month is unfindable, even by exact keyword. Promotion to Semantic (which would rescue it) requires ≥3 accesses — but accesses only happen via recall, and recall is almost never invoked (F8): the promotion path is practically unreachable, so the archive path always wins.

**Fix:** decay should demote memories' *ranking*, never their *findability*. Include Archival in retrieval (rank-penalized), or gate archival on importance rather than idle decay. Re-tune so the half-life of an untouched but once-important fact is measured in months, not weeks.

#### F7. Embedder failure blacks out memory in both directions
`writer.rs:152`: `self.embedder.embed(...).await?` aborts `retain_with_stage` *after* the memory row is upserted but *before* FTS5 indexing (`writer.rs:175`), leaving an orphan invisible to both the vector and keyword arms. On the read side, `retrieval.rs:59` embeds the query with `?` — recall returns an error rather than degrading to FTS5. `LazyFastEmbedder` initializes on first use (ONNX init + ~275 MB model download, `main.rs:554–558`); on an offline box or failed download, memory silently stores unindexed rows and every `memory_search` errors.

**Fix:** FTS-index before embedding; on embed failure store the chunk unembedded and queue a backfill; on query-embed failure fall back to FTS + temporal arms.

#### F8. The model is never told memory exists
`orchestrate.rs:41–143` builds the system prompt: identity, date, security policy, skills, channel context. **No mention of memory.** MEMORY_ARCHITECTURE.md is explicit that "memory is NOT automatically injected — the agent decides when to save and search," but nothing instructs the agent to decide. In practice models call `memory_search` at the start of a conversation approximately never without prompting, so even perfectly stored memories go unread. Retrieval also isn't run automatically against the incoming message. The write side survives only because auto-persist doesn't depend on the model (except on Telegram, F3).

**Fix (minimum):** system-prompt paragraph mandating `memory_search` at task start and `memory_save` for durable user facts. **Fix (proper):** automatic retrieval — run `recall(user_message)` per turn and inject top-K over a relevance floor into the context (see §5).

#### F9. 64 KiB compaction ceiling squanders large context windows
`runner.rs:127–143` clamps the effective limit: `effective_context_limit = min(provider_limit, RUSTYKRAB_COMPACTION_CONTEXT_CEILING=65536)`. On Claude at 200k, compaction fires at ~55.7k — the model runs with less than 30% of its window, tripling compaction frequency, and each compaction is the destructive kind described in F2. The stated rationale (slow local-model prompt eval) is an Ollama concern applied globally.

**Fix:** apply the ceiling only to local providers; let cloud providers use `provider.context_limit()` (with the existing 85% threshold and response reserve).

#### F10. `finalize_session` finalizes nothing
Auto-persisted turns carry `session_id = conv.id` (`orchestrate.rs:214`), but shutdown calls `memory_system.finalize_session(agent_id, session_id)` with a process-global UUID minted at boot (`main.rs:582`, `main.rs:1232`) that no memory ever carried. Working→Episodic promotion therefore rides entirely on the 60-minute idle sweep — which never runs if the process exits or crashes within the idle window. Memories can be stranded in Working stage across restarts.

**Fix:** finalize per-conversation (on conversation idle/close), or have shutdown finalize *all* Working memories for the agent.

### 3.3 P2/P3 — Quality and robustness

- **F11 — Telegram media dropped.** `telegram.rs:450,475`: no text and no caption → message ignored entirely; with a caption, only the caption survives — the photo never reaches the model. Users who send screenshots experience "it ignored what I sent." Also `main.rs:1324–1327` early-returns on any non-`Text` channel message content.
- **F12 — Persistence model.** One JSON blob per conversation (`conversation.rs:59–72`), rewritten wholesale after each run, images embedded as base64 inside the blob. No incremental append, no per-message rows, read-modify-write with last-writer-wins across concurrent paths.
- **F13 — Compaction failure kills the run.** `runner.rs:1244` `self.compact_history(conv).await?` — a summarizer hiccup (rate limit, timeout) fails the whole turn; on Telegram that also means F4's data loss. Should degrade (skip compaction, or fall back to non-LLM trimming) rather than abort.
- **F14 — Memory pollution.** `message_to_turn` (`orchestrate.rs:146+`) retains tool calls and tool results verbatim into memory; `compute_importance` even boosts tool-use turns (+0.15). The memory DB fills with ephemeral tool output that competes with real user facts at recall time. SOTA systems extract salient facts; they don't retain raw tool traffic.
- **F15 — Score conflation.** `retrieval.rs:135` multiplies normalized RRF by `effective_score`, so recency/importance multiplicatively suppress relevant-but-old memories, compounding F6. Additive or staged ranking (relevance first, tie-break on recency) is standard.
- **F16 — Hollow graph arm.** Links are only created by the near-dup detector at cosine ≥ 0.85 (`lifecycle.rs:279`) — i.e., "semantically similar" edges that the semantic arm already finds. Co-occurrence/causal edges in the docs are unimplemented. The graph arm adds RRF weight to near-duplicates rather than new information.
- **F17 — Scale ceiling.** Every recall loads all chunk embeddings for the agent (`retrieval.rs:66`, cached) and brute-forces cosine. Fine to ~10⁵ chunks; a year of auto-persisted turns will pass that. sqlite-vec or HNSW solves it when needed.
- **F18 — Doc drift.** MEMORY_ARCHITECTURE.md describes sliding-window truncation with a never-built `summary_budget_ratio` (the code does LLM compaction), an "in-memory BM25 index" (it's durable SQLite FTS5, rebuilt at boot — `main.rs:588`), and a "conversations sled tree" (it's SQLite). Update or regenerate.

---

## 4. State of the art, briefly

Reference points for what "good" looks like in mid-2026:

- **Context management (Anthropic).** The platform now ships [context editing and compaction natively](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents): *context editing* silently clears old tool results (safest form of compaction — removes bulk without touching reasoning), and *compaction* summarizes the older prefix while the recent tail continues verbatim. Combining a memory tool with context editing improved agent benchmark performance ~39% over baseline in Anthropic's published evals. Claude Code's compactor follows the same shape: protected recent window, summarized prefix, durable scratchpad for state that must survive.
- **Memory (Zep/Graphiti).** [Zep's temporal knowledge graph](https://arxiv.org/abs/2501.13956) is the current published SOTA on LongMemEval (~63.8% vs mem0's ~49% on GPT-4o per [their comparison](https://blog.getzep.com/state-of-the-art-agent-memory/)); its differentiator is exactly what F5 lacks: **facts carry validity intervals** (`valid_from`/`invalid_at`), and a new contradicting fact *closes* the old one rather than being discarded. Entities/edges are LLM-extracted, not regex.
- **Memory (Letta/MemGPT lineage).** Tiered memory: small always-in-context "core memory" blocks the agent edits in place (user profile, current task), plus paged-out archival storage searched on demand. The agent is *aggressively prompted* about when to read/write memory — the opposite of F8.
- **Memory (mem0 and similar).** Extraction-based: an LLM pass distills each exchange into discrete facts with add/update/delete operations against existing memories (update-not-duplicate is the default behavior, resolving F5's class of bug by construction).
- **Retrieval.** Hybrid vector+BM25+RRF (which RustyKrab already has) remains the standard base; graph arms add value only with real entity edges; recency belongs in ranking, not in a findability gate.
- **Benchmarks.** [LongMemEval](https://blog.getzep.com/state-of-the-art-agent-memory/) (and successors like MemoryArena) test exactly the observed failure classes: knowledge updates (F5), temporal reasoning (F6), multi-session recall (F3/F10). Worth adopting a small internal eval in this style.

---

## 5. Recommendations

### Phase 0 — Stop the bleeding (small diffs, this week)

1. **Fix image token estimation** (F1): flat ~1,500 tokens per image; nonzero estimate for audio. *One-line change; removes the forced-compaction loop.*
2. **Unify channel wiring** (F3): make Telegram use `prepare_agent`; delete the divergent setup in `run_agent_interactive`. *Telegram inherits memory + durable recall.*
3. **Persist on every exit path** (F4): save the conversation on timeout/cancel/error in the Telegram handler (mirror Slack).
4. **Raise the compaction ceiling for cloud providers** (F9): ceiling applies to Ollama only.
5. **Keep the recent tail on compaction** (F2): preserve the last N messages (respecting tool-pair boundaries) verbatim; summarize only the prefix; never displace the newest user message.
6. **Prompt the model about memory** (F8): a short system-prompt section — "search memory before answering questions about prior conversations; save durable user facts with memory_save."
7. **Flip near-dup precedence** (F5): newer memory wins; older gets `invalidated_by = newer`.
8. **Don't abort on compaction failure** (F13): log, skip, retry next iteration.

Items 1–4 are wiring/config fixes and safe to ship immediately; 5–8 are behavioral but small.

### Phase 1 — Make memory trustworthy (1–2 weeks)

- **Decouple decay from findability** (F6, F15): everything Episodic+ stays searchable; decay affects rank only. Archive on explicit signals (superseded, user-deleted), not idle time.
- **Graceful embedder degradation** (F7): FTS-index first; queue embedding backfill; recall falls back to FTS+temporal when query embedding fails.
- **Automatic retrieval injection**: run `recall(incoming_message)` each turn, inject top-3–5 results above a relevance floor as a system-adjacent context block. This converts memory from "tool the model never calls" to "ambient recall," matching Letta/Zep deployment practice. Keep the tools for deep search.
- **Fix session lifecycle** (F10): finalize per conversation on idle; shutdown finalizes all Working memories.
- **Stop retaining raw tool traffic** (F14): retain user/assistant text turns; for tool-heavy work retain a distilled outcome line, not payloads.
- **Per-message persistence** (F12): a `messages` table (conversation_id, seq, role, content JSON) appended as messages are pushed; blob kept only as a cache if needed. Store media as files, reference by path (the `FileRef` variant already exists).
- **Docs**: rewrite MEMORY_ARCHITECTURE.md from the code (F18).

### Phase 2 — Rebuild the memory core (accepted option; 3–6 weeks)

If rebuild appetite is real, the highest-leverage rebuild is the **memory write path and fact model**, not the retrieval pipeline (which is already sound):

1. **Extraction-based memory (mem0-style) with temporal validity (Graphiti-style).** Replace verbatim-turn retention with an async LLM extraction pass per exchange that emits typed facts (`subject, predicate, object, valid_from`) and an operation: `ADD`, `UPDATE(supersedes=id)`, or `NOOP(duplicate_of=id)`. Contradiction closes the old fact's validity interval instead of deleting either. Verbatim turns remain in the conversation store (source of truth) — memory becomes a distilled, current-state layer over them. This structurally eliminates F5/F14 and gives "what does the user prefer *now*" semantics.
2. **Real graph edges from extraction**: entities and relations from the extraction pass populate `memory_links`, making the graph arm additive instead of redundant (F16). The existing 4-arm RRF fusion then earns its keep.
3. **Two-tier context à la MemGPT**: a small always-present "core memory" block (user profile + standing preferences + current task state) that the agent edits via a `core_memory_update` tool, rendered into every system prompt; the hybrid store behind it as archival tier. This gives cross-session continuity even when retrieval misses.
4. **Compaction aligned with the platform**: prefix-summary + verbatim tail (Phase 0.5), tool-result clearing before summarization (drop old tool_result bodies first — the cheapest tokens to reclaim, per Anthropic's context-editing model), and the recall archive kept as-is (it's a good RLM-style pattern, once durable everywhere).
5. **An internal LongMemEval-style eval**: ~50 scripted multi-session scenarios (knowledge updates, temporal questions, cross-channel recall) run in CI against the memory API, so regressions like F3/F5 are caught by tests rather than by users.

**Not recommended:** adopting a heavyweight external memory service (Zep/Letta as a sidecar). The security posture of this project (loopback-only, SQLite, no external deps) is a feature; the Graphiti *ideas* port cleanly onto the existing SQLite schema (`valid_from`/`invalid_at` columns already half-exist as `occurred_start`/`invalidated_at`).

---

## 6. Verification checklist for the fixes

- Send a photo + caption via WebChat, then 10 text messages → conversation must not compact (token estimate sane).
- Fill a conversation past the threshold → post-compaction context must contain the last N messages verbatim and the model must answer "what did I just say?" correctly.
- Tell the bot a fact on Telegram, restart the process, ask on Telegram and on WebChat → both must recall it.
- Tell the bot "my X is A", later "my X is actually B" → `memory_search("X")` must return B (and ideally note A superseded).
- Kill the embedder (remove model cache, block network) → `memory_save` succeeds, `memory_search` still returns keyword matches.
- Set a memory's `last_accessed_at` 60 days back → it must still be retrievable by exact keyword.
- Force a summarizer error during compaction → the turn must still complete and the conversation must persist.

## Sources

- [Anthropic — Effective context engineering for AI agents](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents)
- [Zep: A Temporal Knowledge Graph Architecture for Agent Memory (arXiv:2501.13956)](https://arxiv.org/abs/2501.13956)
- [Zep — State of the art in agent memory (LongMemEval results)](https://blog.getzep.com/state-of-the-art-agent-memory/)
- [Agent memory systems compared: Letta, Mem0, Graphiti, Cognee](https://codepointer.substack.com/p/agent-memory-systems-and-knowledge)
