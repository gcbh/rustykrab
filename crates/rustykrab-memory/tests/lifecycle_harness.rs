//! Memory lifecycle harness — one executable check per stage transition.
//!
//! The lifecycle is a state machine whose transitions are driven by wall-clock
//! age, idle time, importance, and access count. `LifecycleManager::sweep`
//! reads `Utc::now()` internally, so this harness drives transitions by
//! *backdating* seeded rows rather than injecting a clock — which has the
//! side benefit of exercising the same code path production uses.
//!
//! Every transition is pinned from both sides: one memory that must move and
//! one just under the threshold that must not. A test that only proves the
//! positive case passes just as well when the predicate is `true`.
//!
//! Stages covered:
//!   admission → write/classify → dedup
//!   Working   → Episodic   (finalize_session, and the sweep safety net)
//!   Episodic  → Semantic   (promotion)
//!   Episodic  → Archival   (demotion)
//!   Archival  → Tombstone  (tombstoning)
//!   Tombstone → purged     (hard delete past retention)
//!   any       → Tombstone  (explicit invalidation)
//!   retrieval (what is reachable at each stage, and access accounting)

use std::sync::Arc;

use chrono::{Duration, Utc};
use rustykrab_memory::admission::{admit, Rejection};
use rustykrab_memory::embedding::ZeroEmbedder;
use rustykrab_memory::storage::{MemoryStorage, SqliteMemoryStorage};
use rustykrab_memory::types::{
    ConversationTurn, ImportanceSource, LifecycleStage, Memory, MemoryScope, TurnMetadata,
};
use rustykrab_memory::{MemoryConfig, MemorySystem};
use uuid::Uuid;

// ── harness ────────────────────────────────────────────────────────

/// Config defaults the harness asserts against. Mirrored here so a change to
/// production defaults fails these tests loudly instead of silently shifting
/// what the suite proves.
const PROMOTE_MIN_ACCESS: u32 = 3;
const PROMOTE_MIN_AGE_DAYS: i64 = 7;
const TOMBSTONE_IDLE_DAYS: i64 = 180;
const TOMBSTONE_IMPORTANCE_MAX: f64 = 0.3;
const WORKING_MAX_IDLE_MINUTES: i64 = 60;
const TOMBSTONE_RETENTION_DAYS: i64 = 30;

struct Harness {
    system: Arc<MemorySystem>,
    storage: Arc<dyn MemoryStorage>,
    agent: Uuid,
}

/// How a seeded memory should be positioned in time and score-space.
#[derive(Clone)]
struct Seed {
    stage: LifecycleStage,
    /// Age since creation.
    age_days: i64,
    /// Idle time (drives decay and the tombstone/demotion clocks).
    idle_days: i64,
    importance: f64,
    access_count: u32,
    session: Uuid,
    content: String,
}

impl Seed {
    fn new(stage: LifecycleStage) -> Self {
        Self {
            stage,
            age_days: 0,
            idle_days: 0,
            importance: 0.5,
            access_count: 0,
            session: Uuid::new_v4(),
            content: format!("seeded memory {}", Uuid::new_v4()),
        }
    }
    fn age_days(mut self, d: i64) -> Self {
        self.age_days = d;
        self
    }
    fn idle_days(mut self, d: i64) -> Self {
        self.idle_days = d;
        self
    }
    fn idle_minutes(mut self, m: i64) -> Self {
        // Represent sub-day idle as a fraction; stored precisely below.
        self.idle_days = -m; // negative marker: minutes, resolved in seed()
        self
    }
    fn importance(mut self, i: f64) -> Self {
        self.importance = i;
        self
    }
    fn access_count(mut self, c: u32) -> Self {
        self.access_count = c;
        self
    }
    fn session(mut self, s: Uuid) -> Self {
        self.session = s;
        self
    }
    fn content(mut self, c: &str) -> Self {
        self.content = c.to_string();
        self
    }
}

impl Harness {
    fn new() -> Self {
        let storage: Arc<dyn MemoryStorage> =
            Arc::new(SqliteMemoryStorage::open_in_memory().expect("in-memory store"));
        let system = Arc::new(MemorySystem::new(
            MemoryConfig::default(),
            Arc::clone(&storage),
            Arc::new(ZeroEmbedder::new(8)),
        ));
        Self {
            system,
            storage,
            agent: Uuid::new_v4(),
        }
    }

    /// Insert a memory positioned exactly as described, bypassing the write
    /// path so a stage/age can be constructed directly.
    async fn seed(&self, s: &Seed) -> Uuid {
        let now = Utc::now();
        let created = now - Duration::days(s.age_days.max(0));
        let idle_delta = if s.idle_days < 0 {
            Duration::minutes(-s.idle_days)
        } else {
            Duration::days(s.idle_days)
        };
        let last_accessed = now - idle_delta;
        let id = Uuid::new_v4();
        let mem = Memory {
            id,
            agent_id: self.agent,
            content: s.content.clone(),
            content_hash: format!("{:x}", md5_like(&s.content)),
            scope: MemoryScope::User,
            session_id: Some(s.session),
            user_id: None,
            lifecycle_stage: s.stage,
            importance: s.importance,
            importance_source: ImportanceSource::Heuristic,
            decay_rate: 1.0,
            confidence: 1.0,
            access_count: s.access_count,
            last_accessed_at: Some(last_accessed),
            last_relevant_at: None,
            created_at: created,
            parent_memory_ids: Vec::new(),
            consolidation_generation: 0,
            proof_count: 1,
            occurred_start: None,
            occurred_end: None,
            is_valid: true,
            invalidated_by: None,
            invalidated_at: None,
            tags: Vec::new(),
            metadata: serde_json::json!({}),
        };
        self.storage.upsert_memory(&mem).await.expect("seed");
        self.storage
            .fts_index(id, self.agent, &s.content)
            .await
            .expect("index");
        id
    }

    async fn stage_of(&self, id: Uuid) -> Option<LifecycleStage> {
        self.storage
            .get_memory(id)
            .await
            .expect("get")
            .map(|m| m.lifecycle_stage)
    }

    async fn exists(&self, id: Uuid) -> bool {
        self.storage.get_memory(id).await.expect("get").is_some()
    }

    async fn sweep(&self) -> rustykrab_memory::lifecycle::LifecycleSweepStats {
        self.system
            .lifecycle_sweep(self.agent)
            .await
            .expect("sweep")
    }
}

/// Cheap deterministic hash so seeded rows get distinct content hashes
/// without pulling in a hashing dependency for the test.
fn md5_like(s: &str) -> u64 {
    s.bytes().fold(1469598103934665603u64, |h, b| {
        (h ^ b as u64).wrapping_mul(1099511628211)
    })
}

fn turn(session: Uuid, content: &str) -> ConversationTurn {
    ConversationTurn {
        id: Uuid::new_v4(),
        session_id: session,
        turn_number: 0,
        speaker: "user".to_string(),
        content: content.to_string(),
        token_count: None,
        metadata: TurnMetadata::default(),
    }
}

// ── STAGE 0: admission ─────────────────────────────────────────────

#[tokio::test]
async fn stage_admission_classifies_content() {
    // Prose is admitted; machine output and loop chatter never enter the store.
    let cases: &[(&str, Option<Rejection>)] = &[
        ("The Maui trip is May 24-31, two travelers.", None),
        ("Budget: $3k", None),
        ("予算は3千ドル", None),
        ("", Some(Rejection::Empty)),
        ("ok", Some(Rejection::TooShort)),
        (
            "tool_call:gmail({\"action\":\"read\"})",
            Some(Rejection::MachineOutput),
        ),
        (
            "{\"count\":0,\"results\":[]}",
            Some(Rejection::MachineOutput),
        ),
        (
            "Multiple consecutive tool calls have failed.",
            Some(Rejection::LoopControl),
        ),
        ("Continue.", Some(Rejection::LoopControl)),
        // Near-misses that must still be admitted.
        ("Current task list: 1. Book Maui flights", None),
        ("Continue. The next step is to book flights.", None),
        ("The API returned {\"ok\": true} so the fix worked.", None),
    ];
    for (content, expected) in cases {
        let got = admit(content).err();
        assert_eq!(
            got, *expected,
            "admission verdict for {content:?} was {got:?}, expected {expected:?}"
        );
    }
}

#[tokio::test]
async fn stage_admission_gates_the_write_path() {
    let h = Harness::new();
    let session = Uuid::new_v4();

    let rejected = h
        .system
        .retain(turn(session, "tool_result:{\"ok\":true}"), h.agent)
        .await
        .expect("retain");
    assert!(rejected.is_none(), "machine output must not be written");

    let admitted = h
        .system
        .retain(
            turn(session, "User prefers aisle seats on long flights."),
            h.agent,
        )
        .await
        .expect("retain");
    assert!(admitted.is_some(), "prose must be written");

    let all = h.storage.list_retrievable(h.agent).await.expect("list");
    assert_eq!(all.len(), 1, "only the admitted memory should exist");
}

// ── STAGE 1: write / classify ──────────────────────────────────────

#[tokio::test]
async fn stage_write_assigns_stage_and_importance() {
    let h = Harness::new();
    let session = Uuid::new_v4();

    let id = h
        .system
        .retain(
            turn(session, "Geoff prefers morning briefings at 7am."),
            h.agent,
        )
        .await
        .expect("retain")
        .expect("admitted");

    let mem = h
        .storage
        .get_memory(id)
        .await
        .expect("get")
        .expect("exists");
    assert_eq!(
        mem.lifecycle_stage,
        LifecycleStage::Working,
        "retain() must enter at Working"
    );
    assert_eq!(mem.access_count, 0, "a fresh write has never been accessed");
    assert_eq!(mem.proof_count, 1, "a fresh write has one proof");
    assert!(
        mem.importance > 0.0 && mem.importance <= 1.0,
        "importance must be scored into [0,1], got {}",
        mem.importance
    );
    assert_eq!(mem.importance_source, ImportanceSource::Heuristic);
    assert_eq!(mem.session_id, Some(session));
}

// ── STAGE 2: dedup ─────────────────────────────────────────────────

#[tokio::test]
async fn stage_dedup_corroborates_without_inflating_retrieval_signal() {
    let h = Harness::new();
    let session = Uuid::new_v4();
    let content = "The Palermo flight departs October 3rd at 14:20.";

    let first = h
        .system
        .retain(turn(session, content), h.agent)
        .await
        .expect("retain")
        .expect("admitted");
    let second = h
        .system
        .retain(turn(session, content), h.agent)
        .await
        .expect("retain")
        .expect("admitted");

    assert_eq!(
        first, second,
        "same-session duplicate collapses onto one row"
    );
    let mem = h
        .storage
        .get_memory(first)
        .await
        .expect("get")
        .expect("exists");
    assert_eq!(mem.proof_count, 2, "duplicate corroborates");
    assert_eq!(mem.access_count, 0, "duplicate is not a retrieval");
    assert!(
        mem.last_relevant_at.is_some(),
        "duplicate refreshes relevance"
    );
}

#[tokio::test]
async fn stage_dedup_keeps_a_row_per_conversation() {
    let h = Harness::new();
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    let content = "What is our Palermo budget?";

    let ida = h
        .system
        .retain(turn(a, content), h.agent)
        .await
        .unwrap()
        .unwrap();
    let idb = h
        .system
        .retain(turn(b, content), h.agent)
        .await
        .unwrap()
        .unwrap();

    assert_ne!(ida, idb, "each conversation keeps its own row");
    let ma = h.storage.get_memory(ida).await.unwrap().unwrap();
    let mb = h.storage.get_memory(idb).await.unwrap().unwrap();
    assert_eq!(ma.session_id, Some(a));
    assert_eq!(mb.session_id, Some(b));
    assert_eq!(ma.proof_count, 2, "the original is still corroborated");
}

// ── STAGE 3: Working → Episodic ────────────────────────────────────

#[tokio::test]
async fn stage_working_to_episodic_via_finalize_session() {
    let h = Harness::new();
    let session = Uuid::new_v4();
    let other = Uuid::new_v4();

    let mine = h
        .seed(&Seed::new(LifecycleStage::Working).session(session))
        .await;
    let theirs = h
        .seed(&Seed::new(LifecycleStage::Working).session(other))
        .await;

    let promoted = h
        .system
        .finalize_session(h.agent, session)
        .await
        .expect("finalize");

    assert_eq!(promoted, 1, "only the named session is finalized");
    assert_eq!(h.stage_of(mine).await, Some(LifecycleStage::Episodic));
    assert_eq!(
        h.stage_of(theirs).await,
        Some(LifecycleStage::Working),
        "another session's working memory is untouched"
    );
}

#[tokio::test]
async fn stage_working_to_episodic_safety_net_respects_idle_threshold() {
    let h = Harness::new();
    // Idle past the threshold → promoted by the sweep even without finalize.
    let stale = h
        .seed(&Seed::new(LifecycleStage::Working).idle_minutes(WORKING_MAX_IDLE_MINUTES + 5))
        .await;
    // Still active → left alone.
    let fresh = h
        .seed(&Seed::new(LifecycleStage::Working).idle_minutes(WORKING_MAX_IDLE_MINUTES - 5))
        .await;

    let stats = h.sweep().await;

    assert_eq!(h.stage_of(stale).await, Some(LifecycleStage::Episodic));
    assert_eq!(
        h.stage_of(fresh).await,
        Some(LifecycleStage::Working),
        "an active session's working memory must survive the sweep"
    );
    assert_eq!(stats.promoted_to_episodic, 1);
}

// ── STAGE 4: Episodic → Semantic ───────────────────────────────────

#[tokio::test]
async fn stage_episodic_to_semantic_requires_both_access_and_age() {
    let h = Harness::new();
    let qualifies = h
        .seed(
            &Seed::new(LifecycleStage::Episodic)
                .age_days(PROMOTE_MIN_AGE_DAYS + 1)
                .access_count(PROMOTE_MIN_ACCESS)
                .importance(0.9),
        )
        .await;
    let too_young = h
        .seed(
            &Seed::new(LifecycleStage::Episodic)
                .age_days(PROMOTE_MIN_AGE_DAYS - 1)
                .access_count(PROMOTE_MIN_ACCESS)
                .importance(0.9),
        )
        .await;
    let too_few_accesses = h
        .seed(
            &Seed::new(LifecycleStage::Episodic)
                .age_days(PROMOTE_MIN_AGE_DAYS + 1)
                .access_count(PROMOTE_MIN_ACCESS - 1)
                .importance(0.9),
        )
        .await;

    let stats = h.sweep().await;

    assert_eq!(h.stage_of(qualifies).await, Some(LifecycleStage::Semantic));
    assert_eq!(h.stage_of(too_young).await, Some(LifecycleStage::Episodic));
    assert_eq!(
        h.stage_of(too_few_accesses).await,
        Some(LifecycleStage::Episodic)
    );
    assert_eq!(stats.promoted_to_semantic, 1);
}

/// Promotion must key on *retrievals*, not re-saves. This is the invariant the
/// proof/access split exists to protect: repeatedly writing the same content
/// must never buy a promotion to the permanent tier.
#[tokio::test]
async fn stage_promotion_is_not_reachable_by_repeated_writes() {
    let h = Harness::new();
    let session = Uuid::new_v4();
    let content = "The daily briefing failed: Gmail credentials are placeholders.";

    let id = h
        .system
        .retain(turn(session, content), h.agent)
        .await
        .unwrap()
        .unwrap();
    // Re-save the identical content many times, as a failing cron job would.
    for _ in 0..10 {
        h.system
            .retain(turn(session, content), h.agent)
            .await
            .unwrap();
    }

    let mem = h.storage.get_memory(id).await.unwrap().unwrap();
    assert_eq!(mem.proof_count, 11, "each re-save corroborates");
    assert!(
        mem.access_count < PROMOTE_MIN_ACCESS,
        "re-saving must not accumulate promotion credit (access_count={})",
        mem.access_count
    );
}

// ── STAGE 5: Episodic → Archival ───────────────────────────────────

#[tokio::test]
async fn stage_episodic_to_archival_demotes_only_decayed_and_idle() {
    let h = Harness::new();
    // Low importance + long idle → effective score collapses below threshold.
    let decayed = h
        .seed(
            &Seed::new(LifecycleStage::Episodic)
                .age_days(60)
                .idle_days(45)
                .importance(0.3),
        )
        .await;
    // Same idle, but high importance decays ~5x slower → stays hot.
    let important = h
        .seed(
            &Seed::new(LifecycleStage::Episodic)
                .age_days(60)
                .idle_days(45)
                .importance(0.95),
        )
        .await;
    // Low importance but recently used → the idle gate protects it.
    let recent = h
        .seed(
            &Seed::new(LifecycleStage::Episodic)
                .age_days(60)
                .idle_days(5)
                .importance(0.3),
        )
        .await;

    let stats = h.sweep().await;

    assert_eq!(h.stage_of(decayed).await, Some(LifecycleStage::Archival));
    assert_eq!(
        h.stage_of(important).await,
        Some(LifecycleStage::Episodic),
        "importance must slow decay enough to survive"
    );
    assert_eq!(
        h.stage_of(recent).await,
        Some(LifecycleStage::Episodic),
        "recent use must prevent demotion regardless of importance"
    );
    assert_eq!(stats.demoted_to_archival, 1);
}

// ── STAGE 6: Archival → Tombstone ──────────────────────────────────

#[tokio::test]
async fn stage_archival_to_tombstone_requires_idle_and_low_importance() {
    let h = Harness::new();
    let forgettable = h
        .seed(
            &Seed::new(LifecycleStage::Archival)
                .idle_days(TOMBSTONE_IDLE_DAYS + 1)
                .importance(TOMBSTONE_IMPORTANCE_MAX - 0.05),
        )
        .await;
    let idle_but_important = h
        .seed(
            &Seed::new(LifecycleStage::Archival)
                .idle_days(TOMBSTONE_IDLE_DAYS + 1)
                .importance(TOMBSTONE_IMPORTANCE_MAX + 0.05),
        )
        .await;
    let unimportant_but_recent = h
        .seed(
            &Seed::new(LifecycleStage::Archival)
                .idle_days(TOMBSTONE_IDLE_DAYS - 1)
                .importance(TOMBSTONE_IMPORTANCE_MAX - 0.05),
        )
        .await;

    let stats = h.sweep().await;

    assert_eq!(
        h.stage_of(forgettable).await,
        Some(LifecycleStage::Tombstone)
    );
    assert_eq!(
        h.stage_of(idle_but_important).await,
        Some(LifecycleStage::Archival),
        "importance above the floor must prevent tombstoning"
    );
    assert_eq!(
        h.stage_of(unimportant_but_recent).await,
        Some(LifecycleStage::Archival),
        "idle below the window must prevent tombstoning"
    );
    assert_eq!(stats.tombstoned, 1);
}

// ── STAGE 7: Tombstone → purged ────────────────────────────────────

#[tokio::test]
async fn stage_purge_hard_deletes_only_tombstones_past_retention() {
    let h = Harness::new();
    let old_tombstone = h
        .seed(
            &Seed::new(LifecycleStage::Tombstone)
                .age_days(TOMBSTONE_RETENTION_DAYS + 5)
                .idle_days(TOMBSTONE_RETENTION_DAYS + 5),
        )
        .await;
    let recent_tombstone = h
        .seed(
            &Seed::new(LifecycleStage::Tombstone)
                .age_days(TOMBSTONE_RETENTION_DAYS - 5)
                .idle_days(TOMBSTONE_RETENTION_DAYS - 5),
        )
        .await;
    let old_archival = h
        .seed(
            &Seed::new(LifecycleStage::Archival)
                .age_days(TOMBSTONE_RETENTION_DAYS + 5)
                .idle_days(1)
                .importance(0.9),
        )
        .await;

    let stats = h.sweep().await;

    assert!(
        !h.exists(old_tombstone).await,
        "expired tombstone is purged"
    );
    assert!(
        h.exists(recent_tombstone).await,
        "tombstone inside retention must survive for audit"
    );
    assert!(
        h.exists(old_archival).await,
        "purge must only touch tombstones"
    );
    assert_eq!(stats.purged, 1);
}

// ── STAGE 8: explicit invalidation ─────────────────────────────────

#[tokio::test]
async fn stage_invalidation_tombstones_and_removes_from_retrieval() {
    let h = Harness::new();
    let id = h
        .seed(&Seed::new(LifecycleStage::Semantic).content("Wailea condo booked for May 24"))
        .await;

    h.system
        .invalidate_memory(id, None)
        .await
        .expect("invalidate");

    assert_eq!(h.stage_of(id).await, Some(LifecycleStage::Tombstone));
    let retrievable = h.storage.list_retrievable(h.agent).await.expect("list");
    assert!(
        !retrievable.iter().any(|m| m.id == id),
        "an invalidated memory must leave the hot set"
    );
}

// ── STAGE 9: retrieval ─────────────────────────────────────────────

#[tokio::test]
async fn stage_retrieval_reaches_only_hot_stages() {
    let h = Harness::new();
    let working = h
        .seed(&Seed::new(LifecycleStage::Working).content("alpha topic"))
        .await;
    let episodic = h
        .seed(&Seed::new(LifecycleStage::Episodic).content("alpha topic"))
        .await;
    let semantic = h
        .seed(&Seed::new(LifecycleStage::Semantic).content("alpha topic"))
        .await;
    let archival = h
        .seed(&Seed::new(LifecycleStage::Archival).content("alpha topic"))
        .await;
    let tombstone = h
        .seed(&Seed::new(LifecycleStage::Tombstone).content("alpha topic"))
        .await;

    let hits = h
        .system
        .recall("alpha topic", h.agent, 50)
        .await
        .expect("recall");
    let found: Vec<Uuid> = hits.iter().map(|r| r.memory_id).collect();

    for (id, stage) in [
        (working, "Working"),
        (episodic, "Episodic"),
        (semantic, "Semantic"),
    ] {
        assert!(found.contains(&id), "{stage} must be retrievable");
    }
    for (id, stage) in [(archival, "Archival"), (tombstone, "Tombstone")] {
        assert!(
            !found.contains(&id),
            "{stage} must be excluded from retrieval"
        );
    }
}

#[tokio::test]
async fn stage_retrieval_records_access_on_returned_results_only() {
    let h = Harness::new();
    let hit = h
        .seed(&Seed::new(LifecycleStage::Episodic).content("kayak reservation"))
        .await;
    let miss = h
        .seed(&Seed::new(LifecycleStage::Episodic).content("unrelated dentist appointment"))
        .await;

    let _ = h
        .system
        .recall("kayak reservation", h.agent, 1)
        .await
        .expect("recall");

    let hit_mem = h.storage.get_memory(hit).await.unwrap().unwrap();
    let miss_mem = h.storage.get_memory(miss).await.unwrap().unwrap();
    assert_eq!(
        hit_mem.access_count, 1,
        "a returned result records an access"
    );
    assert_eq!(
        miss_mem.access_count, 0,
        "a result that was not returned must not be counted as accessed"
    );
}

#[tokio::test]
async fn stage_retrieval_scoped_to_session_excludes_and_does_not_touch_others() {
    let h = Harness::new();
    let mine = Uuid::new_v4();
    let theirs = Uuid::new_v4();
    let in_session = h
        .seed(
            &Seed::new(LifecycleStage::Episodic)
                .session(mine)
                .content("Palermo trip departs October 3rd"),
        )
        .await;
    let out_of_session = h
        .seed(
            &Seed::new(LifecycleStage::Episodic)
                .session(theirs)
                .content("Palermo trip hotel is near the opera house"),
        )
        .await;

    let hits = h
        .system
        .recall_in_session("Palermo trip", h.agent, 10, mine)
        .await
        .expect("scoped recall");

    let found: Vec<Uuid> = hits.iter().map(|r| r.memory_id).collect();
    assert!(
        found.contains(&in_session),
        "in-session memory must be found"
    );
    assert!(
        !found.contains(&out_of_session),
        "out-of-session memory must be excluded"
    );

    let other = h.storage.get_memory(out_of_session).await.unwrap().unwrap();
    assert_eq!(
        other.access_count, 0,
        "filtered-out memories must receive no phantom access"
    );
}

// ── whole-lifecycle walk ───────────────────────────────────────────

/// Drive one memory through every stage in order, asserting the state after
/// each step. This is the end-to-end shape the individual tests decompose.
#[tokio::test]
async fn full_lifecycle_walk_working_to_purged() {
    let h = Harness::new();
    let session = Uuid::new_v4();

    // 1. admitted, enters Working
    let id = h
        .system
        .retain(
            turn(session, "Booked the Wailea condo for May 24-31."),
            h.agent,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(h.stage_of(id).await, Some(LifecycleStage::Working));

    // 2. session ends → Episodic
    h.system.finalize_session(h.agent, session).await.unwrap();
    assert_eq!(h.stage_of(id).await, Some(LifecycleStage::Episodic));

    // 3. age it out unused → Archival
    let mut mem = h.storage.get_memory(id).await.unwrap().unwrap();
    mem.created_at = Utc::now() - Duration::days(60);
    mem.last_accessed_at = Some(Utc::now() - Duration::days(45));
    mem.importance = 0.2;
    h.storage.upsert_memory(&mem).await.unwrap();
    h.sweep().await;
    assert_eq!(h.stage_of(id).await, Some(LifecycleStage::Archival));

    // 4. leave it idle past the tombstone window → Tombstone
    let mut mem = h.storage.get_memory(id).await.unwrap().unwrap();
    mem.last_accessed_at = Some(Utc::now() - Duration::days(TOMBSTONE_IDLE_DAYS + 1));
    h.storage.upsert_memory(&mem).await.unwrap();
    h.sweep().await;
    assert_eq!(h.stage_of(id).await, Some(LifecycleStage::Tombstone));
    assert!(
        !h.storage
            .list_retrievable(h.agent)
            .await
            .unwrap()
            .iter()
            .any(|m| m.id == id),
        "a tombstoned memory is unreachable by retrieval"
    );

    // 5. past retention → hard-deleted.
    // Retention ages from `invalidated_at` (stamped when the sweep tombstoned
    // it), NOT from creation — so age the transition, not the memory.
    let mut mem = h.storage.get_memory(id).await.unwrap().unwrap();
    assert!(
        mem.invalidated_at.is_some(),
        "tombstoning must stamp invalidated_at so retention has an anchor"
    );
    mem.invalidated_at = Some(Utc::now() - Duration::days(TOMBSTONE_RETENTION_DAYS + 5));
    h.storage.upsert_memory(&mem).await.unwrap();
    h.sweep().await;
    assert!(!h.exists(id).await, "memory is purged at end of life");
}

/// Retention is measured from the moment a memory was tombstoned, not from
/// when it was created — otherwise a long-lived memory would be hard-deleted
/// the instant it was tombstoned, destroying the audit window.
#[tokio::test]
async fn stage_purge_ages_from_tombstone_time_not_creation_time() {
    let h = Harness::new();
    // Created long ago, but only just tombstoned.
    let just_tombstoned = h
        .seed(
            &Seed::new(LifecycleStage::Archival)
                .age_days(TOMBSTONE_RETENTION_DAYS * 10)
                .idle_days(TOMBSTONE_IDLE_DAYS + 1)
                .importance(TOMBSTONE_IMPORTANCE_MAX - 0.05),
        )
        .await;

    // First sweep tombstones it and stamps invalidated_at = now.
    h.sweep().await;
    assert_eq!(
        h.stage_of(just_tombstoned).await,
        Some(LifecycleStage::Tombstone)
    );

    // Second sweep must NOT purge it: it is ancient by creation date but
    // brand new as a tombstone.
    let stats = h.sweep().await;
    assert_eq!(stats.purged, 0);
    assert!(
        h.exists(just_tombstoned).await,
        "a freshly tombstoned memory must survive its audit window regardless of age"
    );
}
