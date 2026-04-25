# Bugs and dead code — `tools`, `memory`, `skills`

Severity: `[C]` critical, `[H]` high, `[M]` medium, `[L]` low.

---

## Tools — paths and security

### T-1  [C] `validate_path` symlink TOCTOU
See `02-security.md` §S-15.

### T-2  [C] DNS rebinding across HTTP tools
See `01-critical.md` §C-1 / `02-security.md` §S-1.

### T-3  [C] HTTP redirects bypass SSRF
See `01-critical.md` §C-2 / `02-security.md` §S-2.

### T-4  [C] Obsidian `danger_accept_invalid_certs(true)`
See `01-critical.md` §C-4. Compounded by per-call client construction
at `obsidian.rs:583,637` (S-6).

### T-5  [C] Exec/Process command validator allows `KEY=val` prefix
See `01-critical.md` §C-5.

---

## Tools — argument / schema mismatches

### T-6  [H] `credential_read` schema lists fields as required that the impl treats as optional
`crates/rustykrab-tools/src/credential_read.rs:54,75`. The schema
states `required: ["action", "source", "service", "account"]` but
`source = "store"` ignores `service`/`account`, and `action = "list"`
ignores them entirely. Make schema conditional, or supply defaults so
the contract is honoured.

### T-7  [M] `read` tool offset/limit accept negatives via silent coercion
`crates/rustykrab-tools/src/read.rs:117-118`. `args["offset"].as_u64().unwrap_or(0)`
on `-1` returns `None`; treated as 0. Use `as_i64().filter(|v| *v >= 0)`.

### T-8  [M] `memory_search` doesn’t validate backend return shape
`crates/rustykrab-tools/src/memory_search.rs:72-78`. Non-array result
silently becomes 0 results, hiding errors. Type-check explicitly.

### T-9  [M] `cron` tool passes `schedule` to backend without validating
`crates/rustykrab-tools/src/cron.rs:96-114`. Validate cron expr or
ISO 8601 timestamp at submit time so the agent gets a clear error.

### T-10 [M] `web_search` URL decoder produces invalid UTF-8 for `%FF`-style escapes
`crates/rustykrab-tools/src/web_search.rs:284-286`. See `02-security.md`
§S-19.

### T-11 [L] `apply_patch` hunk start of 0 is treated as line 0 (off-by-one)
`crates/rustykrab-tools/src/apply_patch.rs:120-150`. Most diffs avoid
`@@ -0,…`, but malformed inputs can trigger this. Handle explicitly.

### T-12 [L] `apply_patch` doesn’t verify context lines
`crates/rustykrab-tools/src/apply_patch.rs:164-188`. The hunk is
applied based on diff line types without checking that the surrounding
context actually matches. CRLF/LF mismatches silently relocate the
edit.

### T-13 [L] `web_fetch` truncation suffix overflows the cap
`crates/rustykrab-tools/src/web_fetch.rs:118-133`. Subtract suffix
length from budget before truncation.

### T-14 [L] `image` accepts arbitrary bytes as "image"
See `02-security.md` §S-5.

### T-15 [L] `http_request` body uses `String::from_utf8_lossy`, mangling binaries
`crates/rustykrab-tools/src/http_request.rs:105-132`. Either return
bytes / base64, or refuse based on content-type.

### T-16 [L] `memory_save` has no upper bound on tag count
`crates/rustykrab-tools/src/memory_save.rs:73-76`. Cap at e.g. 20.

### T-17 [L] `video` width/height cast `u64 -> u32` truncates silently
Approx. `crates/rustykrab-tools/src/video.rs:129-135`. Validate
`<= u32::MAX as u64`.

### T-18 [L] `exec` PATH fallback is hardcoded
`crates/rustykrab-tools/src/exec.rs:277-288`. Document the fallback or
fail explicitly when `PATH` is unset.

---

## Memory — retrieval math

### M-1  [H] RRF normalization can divide by zero
`crates/rustykrab-memory/src/retrieval.rs:124-133`. If all RRF
weights are zero (or `rrf_k` is zero, which the validator catches),
`max_rrf == 0.0` and the normalization produces `NaN`/`inf`. Add a
post-validate that at least one weight is positive, and guard the
division with `if max_rrf <= 0.0 { 0.0 } else { … }`.

### M-2  [H] Cosine similarity returns 0 on dimension mismatch with only a warning
`crates/rustykrab-memory/src/embedding.rs:23-48`. A live embedding-
model swap silently masks all matches as "irrelevant". Promote to
`error!` and surface a metric. Optionally, refuse to score and fall
back to BM25 alone.

### M-3  [M] `effective_score` can go negative
`crates/rustykrab-memory/src/types.rs:120-134`. Cosine can be in
`[-1, 1]`; if any factor underflows, the product becomes negative.
Clamp the final value to `>= 0.0`.

### M-4  [M] `sentiment_intensity` chained additions can drift outside `[0,1]` before final `clamp`
`crates/rustykrab-memory/src/scoring.rs:106`. Already clamped, but if
any contributor returns NaN (e.g. on empty content), the clamp passes
NaN through (`NaN.clamp(0,1) == NaN`). Add `is_finite()` guard.

### M-5  [M] `MemoryConfig::validate()` only checks two fields
`crates/rustykrab-memory/src/config.rs:69-86`. Validate `decay_rate >
0`, `0 <= default_importance <= 1`, RRF weights non-negative with at
least one positive (cross-ref M-1).

### M-6  [M] `retrieve_temporal` doesn’t verify returned memories belong to the calling agent
`crates/rustykrab-memory/src/retrieval.rs:254-273`. Defense in depth:
assert `mem.agent_id == agent_id`.

### M-7  [L] `MemoryChunk::chunk_index: u32` underdimensioned for pathological inputs
`crates/rustykrab-memory/src/types.rs:144`. Document the limit or
widen.

---

## Memory — concurrency

### M-8  [M] In-memory indices behind `RwLock` with no contention metrics
General `crates/rustykrab-memory/src/*.rs`. Reads dominate; this is
fine. Track lock-wait time so regressions surface.

### M-9  [L] Storage `INSERT OR REPLACE` patterns inherit A-9 race risk
Same fix as `03-bugs-core-agent.md` §A-9.

---

## Skills

### Sk-1 [C] `verify_skill_bundle` lacks canonicalization / domain separator
See `01-critical.md` §C-3.

### Sk-2 [H] No timestamp / version binding on signed payload
See `02-security.md` §S-22.

### Sk-3 [M] `SkillRegistry` lock acquisition uses `.expect("poisoned")`
`crates/rustykrab-skills/src/skill.rs:56,62,71,78,84,90,95`. One
panicking thread → registry permanently unusable. Convert to `Result`
and propagate.

### Sk-4 [M] `parse_skill_md` accepts empty `name` in frontmatter
`crates/rustykrab-skills/src/skill_md.rs:71-99`. `---\n{}\n---\n`
yields a skill with `name = ""`. Reject.

### Sk-5 [L] No replay/revocation list for trusted publisher keys
Pair Sk-2 with a CRL distributed alongside the trust set; revoked
hashes are rejected at verify time.

