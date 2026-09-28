//! Routing by record (plan sections 2, 5 and 10): which worker may take a
//! class of coding work, read from the record the controller writes from
//! verified results.
//!
//! **Classes.** [`work_class`] names the class an item belongs to: `code`
//! for a `code` item and `capability:build` for a capability item the
//! ladder's build rung filed. Other work is not routed by record: every
//! worker whose capabilities cover it qualifies, cheapest first, as in
//! Phase 1.
//!
//! **Qualifying.** Each routed class has a default tier: the policy's
//! prior ([`RoutingPolicy::default_tiers`]), seeded into the store's
//! `routing_defaults` and read back from there, so an accepted routing
//! proposal that moves it (Phase 6) takes effect. A worker whose cost tier is at or
//! above it qualifies by that prior, on probation; a cheaper worker
//! qualifies only once its record has earned the class
//! ([`RoutingPolicy::earn_after`] verified results and its last result
//! verified). The match step then prefers the cheapest qualifying tier.
//! `code` starts at tier 0, so a local worker takes a code slice under
//! probation (plan section 2); `capability:build` starts at the coding
//! agents' tier, so building a tool goes to Codex or Claude Code until a
//! cheaper worker's record says otherwise.
//!
//! **Recording.** Every judged result of a routed class is written to the
//! producing worker's record for that class ([`RecordRouting::record`]):
//! verified done, claimed but not verified, failed, the repairs it took
//! before it verified, and its cost. The controller never moves a class's
//! default tier: that is a routing proposal dreaming files from these
//! records (Phase 6), in either direction.
//!
//! **Escalation** is the ladder's, per item: a failed verification climbs
//! to the switch rung, which excludes the failing worker's tier and every
//! cheaper one, so the next run goes up a kind (plan section 8).

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rustykrab_core::work::{
    ArtifactRef, CapabilityMode, WorkItem, WorkItemId, WorkKind, WorkerKind,
};
use rustykrab_core::Error;
use rustykrab_store::ClassRecord;
use serde::{Deserialize, Serialize};

use crate::controller::Routing;
use crate::errors::GapKind;
use crate::registry::WorkerRegistry;
use crate::worker::{RunUsage, Worker};

/// The class of a `code` item.
pub const CODE: &str = "code";
/// The class of a capability build.
pub const CAPABILITY_BUILD: &str = "capability:build";

/// The artifact-ref kind a ladder-filed capability item carries: the need
/// it answers, as `<gap>:<subject>` (`tool:tide_table`).
pub const CAPABILITY_REF: &str = "capability";

/// The need a ladder-filed capability item answers: which gap, and what is
/// missing. Whether the item builds, acquires or requests it is not here:
/// that is the item's review facet ([`CapabilityMode`] in
/// `work_item_facets`), the one source of truth the review projection,
/// routing and verification all read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityRef {
    pub gap: GapKind,
    pub subject: String,
}

impl CapabilityRef {
    pub fn to_ref(&self) -> ArtifactRef {
        ArtifactRef {
            kind: CAPABILITY_REF.to_string(),
            value: format!("{}:{}", self.gap.as_str(), self.subject),
        }
    }

    /// Read one ref, if it is a capability ref.
    pub fn parse(r: &ArtifactRef) -> Option<CapabilityRef> {
        if r.kind != CAPABILITY_REF {
            return None;
        }
        let (gap, subject) = r.value.split_once(':')?;
        Some(CapabilityRef {
            gap: GapKind::parse(gap.trim())?,
            subject: subject.trim().to_string(),
        })
        .filter(|c| !c.subject.is_empty())
    }

    /// The need among `refs`, if any.
    pub fn of_refs(refs: &[ArtifactRef]) -> Option<CapabilityRef> {
        refs.iter().find_map(CapabilityRef::parse)
    }
}

/// The tool a capability item builds: its facet says `build` and its need
/// is a tool. `None` for anything else.
pub fn built_tool(mode: Option<CapabilityMode>, refs: &[ArtifactRef]) -> Option<String> {
    if mode != Some(CapabilityMode::Build) {
        return None;
    }
    CapabilityRef::of_refs(refs)
        .filter(|c| c.gap == GapKind::Tool)
        .map(|c| c.subject)
}

/// The routed class `item` belongs to, if any. `mode` is a capability
/// item's facet; an item without one is not a build.
pub fn work_class(item: &WorkItem, mode: Option<CapabilityMode>) -> Option<String> {
    match item.kind {
        WorkKind::Code => Some(CODE.to_string()),
        WorkKind::Capability if mode == Some(CapabilityMode::Build) => {
            Some(CAPABILITY_BUILD.to_string())
        }
        _ => None,
    }
}

/// How a judged result counts in the record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Verified from evidence and closed `done`.
    Verified,
    /// Claimed done and failed verification.
    NotVerified,
    /// Ended in an error the worker reported or caused.
    Failed,
}

/// One judged result of a routed class, for the producing worker's record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Judged {
    pub worker: String,
    pub worker_kind: WorkerKind,
    pub item: WorkItemId,
    pub class: String,
    pub verdict: Verdict,
    /// Repair rungs the item climbed before this result.
    pub repairs: u32,
    /// Wall time of the run: the worker's own measure when it keeps one,
    /// else lease to result.
    pub wall_seconds: u64,
    /// What the worker reported spending ([`Worker::usage`]), when it does.
    pub usage: Option<RunUsage>,
    pub at: DateTime<Utc>,
}

/// The routing policy: each routed class's default tier, and what earns a
/// cheaper worker the class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingPolicy {
    /// The lowest cost tier that qualifies for a class without an earned
    /// record. Moving one is a routing proposal (Phase 6), not the
    /// controller's call.
    pub default_tiers: BTreeMap<String, u32>,
    /// Verified results a worker below a class's default tier needs, with
    /// its last result verified, before it qualifies; also the count that
    /// ends probation.
    pub earn_after: u64,
}

impl Default for RoutingPolicy {
    fn default() -> Self {
        RoutingPolicy {
            default_tiers: BTreeMap::from([
                (CODE.to_string(), 0),
                (
                    CAPABILITY_BUILD.to_string(),
                    crate::controller::default_cost_tier(WorkerKind::Codex),
                ),
            ]),
            earn_after: 3,
        }
    }
}

/// Apply one judged result to a class record (the rule the store's
/// transaction runs).
pub fn apply(record: &mut ClassRecord, judged: &Judged, earn_after: u64) {
    match judged.verdict {
        Verdict::Verified => {
            record.verified_done += 1;
            record.repairs += u64::from(judged.repairs);
            record.last_verified_at = Some(judged.at);
        }
        Verdict::NotVerified => {
            record.claimed_not_verified += 1;
            record.last_failed_at = Some(judged.at);
        }
        Verdict::Failed => {
            record.failed += 1;
            record.last_failed_at = Some(judged.at);
        }
    }
    record.note_item(&judged.item);
    record.cost.runs += 1;
    record.cost.wall_seconds += judged.wall_seconds;
    if let Some(usage) = judged.usage {
        record.cost.tokens += usage.tokens;
    }
    record.probation = judged.verdict != Verdict::Verified || record.verified_done < earn_after;
}

/// [`Routing`] over the registry's records: the Phase 3 routing.
pub struct RecordRouting {
    registry: Arc<WorkerRegistry>,
    policy: RoutingPolicy,
    /// Each class's default tier as the store holds it, over the policy's.
    tiers: RwLock<BTreeMap<String, u32>>,
}

impl RecordRouting {
    pub fn new(registry: Arc<WorkerRegistry>) -> RecordRouting {
        RecordRouting::with_policy(registry, RoutingPolicy::default())
    }

    pub fn with_policy(registry: Arc<WorkerRegistry>, policy: RoutingPolicy) -> RecordRouting {
        RecordRouting {
            registry,
            tiers: RwLock::new(policy.default_tiers.clone()),
            policy,
        }
    }

    pub fn policy(&self) -> &RoutingPolicy {
        &self.policy
    }

    /// Seed the store's `routing_defaults` with the policy's prior (a class
    /// already recorded keeps its tier) and read them back.
    pub async fn load(&self) -> Result<(), Error> {
        let store = self.registry.store().workers();
        for (class, tier) in &self.policy.default_tiers {
            store.seed_default_tier(class, *tier).await?;
        }
        self.reload().await
    }

    /// Read the default tiers again, so a moved default takes effect.
    pub async fn reload(&self) -> Result<(), Error> {
        let mut tiers = self.policy.default_tiers.clone();
        for row in self.registry.store().workers().default_tiers().await? {
            tiers.insert(row.class, row.tier);
        }
        *self.tiers.write().unwrap_or_else(|e| e.into_inner()) = tiers;
        Ok(())
    }

    /// A class's default tier, if it is routed.
    pub fn default_tier(&self, class: &str) -> Option<u32> {
        self.tiers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(class)
            .copied()
    }
}

#[async_trait]
impl Routing for RecordRouting {
    fn qualifies(&self, worker: &dyn Worker, _item: &WorkItem, class: Option<&str>) -> bool {
        let Some(class) = class else {
            return true;
        };
        let Some(floor) = self.default_tier(class) else {
            return true;
        };
        if self.cost_tier(worker) >= floor {
            return true;
        }
        self.registry
            .record_of(worker.name(), class)
            .is_some_and(|r| r.verified_done >= self.policy.earn_after && !r.probation)
    }

    fn cost_tier(&self, worker: &dyn Worker) -> u32 {
        self.registry.cost_tier(worker)
    }

    async fn defaults_moved(&self) {
        if let Err(e) = self.reload().await {
            tracing::warn!(error = %e, "routing defaults not reloaded");
        }
    }

    async fn record(&self, judged: &Judged) {
        let earn_after = self.policy.earn_after;
        let entry = judged.clone();
        let class = judged.class.clone();
        let written = self
            .registry
            .update_record(&judged.worker, move |record| {
                apply(record.entry(class).or_default(), &entry, earn_after);
            })
            .await;
        // Pick up a default an accepted proposal moved meanwhile.
        if let Err(e) = self.reload().await {
            tracing::warn!(error = %e, "routing defaults not reloaded");
        }
        if let Err(e) = written {
            tracing::warn!(worker = %judged.worker, item = %judged.item, error = %e,
                "routing record not written");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustykrab_core::work::ResultReport;
    use rustykrab_core::Error;
    use rustykrab_store::Store;

    use crate::worker::{Brief, WorkerCapabilities};

    struct W(&'static str, WorkerKind);

    #[async_trait]
    impl Worker for W {
        fn name(&self) -> &str {
            self.0
        }
        fn kind(&self) -> WorkerKind {
            self.1
        }
        fn capabilities(&self) -> WorkerCapabilities {
            WorkerCapabilities::default()
        }
        async fn run(&self, _brief: Brief) -> Result<ResultReport, Error> {
            Ok(ResultReport::default())
        }
    }

    pub(crate) fn item(kind: WorkKind, refs: Vec<ArtifactRef>) -> WorkItem {
        WorkItem {
            id: "i1".to_string(),
            kind,
            title: "t".to_string(),
            objective: "o".to_string(),
            done_when: "d".to_string(),
            constraints: vec![],
            decisions_made: vec![],
            artifact_refs: refs,
            required_tools: vec![],
            required_mcp_servers: vec![],
            worker_kind: WorkerKind::Any,
            writable_resources: vec![],
            parent: None,
            inputs_from: vec![],
            origin_conversation_id: None,
            trigger: rustykrab_core::work::Trigger::Now,
            preconditions: vec![],
            expires_at: None,
            budget: Default::default(),
            priority: 0,
            status: rustykrab_core::work::Status::Queued,
            status_origin: None,
            plan_id: None,
            held_by: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            closed_at: None,
        }
    }

    fn judged(worker: &str, verdict: Verdict) -> Judged {
        Judged {
            worker: worker.to_string(),
            worker_kind: WorkerKind::Local,
            item: "i1".to_string(),
            class: CODE.to_string(),
            verdict,
            repairs: 1,
            wall_seconds: 30,
            usage: Some(RunUsage {
                tokens: 100,
                wall_ms: 30_000,
                iterations: 2,
                reminders: 0,
            }),
            at: Utc::now(),
        }
    }

    #[test]
    fn the_mode_is_the_facets_and_the_need_is_the_refs() {
        let code = item(WorkKind::Code, vec![]);
        assert_eq!(work_class(&code, None).as_deref(), Some(CODE));
        assert_eq!(work_class(&item(WorkKind::Personal, vec![]), None), None);
        let need = CapabilityRef {
            gap: GapKind::Tool,
            subject: "tide_table".to_string(),
        };
        assert_eq!(need.to_ref().value, "tool:tide_table");
        let cap = item(WorkKind::Capability, vec![need.to_ref()]);
        assert_eq!(CapabilityRef::of_refs(&cap.artifact_refs), Some(need));
        let build = Some(CapabilityMode::Build);
        assert_eq!(work_class(&cap, build).as_deref(), Some(CAPABILITY_BUILD));
        assert_eq!(
            built_tool(build, &cap.artifact_refs).as_deref(),
            Some("tide_table")
        );
        // The same need acquired, or with no facet, is not a build.
        for mode in [
            Some(CapabilityMode::Acquire),
            Some(CapabilityMode::Request),
            None,
        ] {
            assert_eq!(work_class(&cap, mode), None, "{mode:?}");
            assert_eq!(built_tool(mode, &cap.artifact_refs), None);
        }
        // A build whose need is not a tool builds no tool.
        let install = item(
            WorkKind::Capability,
            vec![CapabilityRef {
                gap: GapKind::Install,
                subject: "ffmpeg".into(),
            }
            .to_ref()],
        );
        assert_eq!(built_tool(build, &install.artifact_refs), None);
        assert_eq!(
            CapabilityRef::parse(&ArtifactRef {
                kind: CAPABILITY_REF.into(),
                value: "tool:".into()
            }),
            None
        );
    }

    #[test]
    fn a_record_counts_verdicts_repairs_and_cost_and_probation_ends_on_evidence() {
        let mut record = ClassRecord::default();
        apply(&mut record, &judged("krabby", Verdict::Verified), 2);
        assert_eq!(record.verified_done, 1);
        assert_eq!(record.repairs, 1);
        assert!(record.probation);
        apply(&mut record, &judged("krabby", Verdict::Verified), 2);
        assert!(!record.probation, "earned after two");
        apply(&mut record, &judged("krabby", Verdict::NotVerified), 2);
        assert!(record.probation, "a failed verification puts it back");
        apply(&mut record, &judged("krabby", Verdict::Failed), 2);
        assert_eq!(
            (record.claimed_not_verified, record.failed, record.cost.runs),
            (1, 1, 4)
        );
        assert_eq!(record.cost.wall_seconds, 120);
        assert_eq!(record.cost.tokens, 400);
        assert_eq!(
            record.repairs, 2,
            "repairs count only toward verified results"
        );
        assert_eq!(record.recent_items, ["i1"], "one item, noted once");
        for n in 0..12 {
            let mut other = judged("krabby", Verdict::Verified);
            other.item = format!("i{n}");
            apply(&mut record, &other, 2);
        }
        assert_eq!(record.recent_items.len(), rustykrab_store::RECENT_ITEMS);
        assert_eq!(record.recent_items.last().map(String::as_str), Some("i11"));
    }

    #[tokio::test]
    async fn a_cheaper_worker_qualifies_only_by_default_tier_or_an_earned_record() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("db"), vec![7u8; 32]).unwrap();
        let local: Arc<dyn Worker> = Arc::new(W("krabby", WorkerKind::Local));
        let claude: Arc<dyn Worker> = Arc::new(W("pinch", WorkerKind::ClaudeCode));
        let registry = Arc::new(WorkerRegistry::fixed(
            store,
            vec![local.clone(), claude.clone()],
        ));
        let routing = RecordRouting::new(registry.clone());
        let code = item(WorkKind::Code, vec![]);
        let build = item(WorkKind::Capability, vec![]);
        let personal = item(WorkKind::Personal, vec![]);
        let (code_class, build_class) = (Some(CODE), Some(CAPABILITY_BUILD));
        assert!(
            routing.qualifies(local.as_ref(), &code, code_class),
            "code starts at tier 0"
        );
        assert!(routing.qualifies(local.as_ref(), &personal, None));
        assert!(
            !routing.qualifies(local.as_ref(), &build, build_class),
            "builds start higher"
        );
        assert!(routing.qualifies(claude.as_ref(), &build, build_class));

        let mut earned = judged("krabby", Verdict::Verified);
        earned.class = CAPABILITY_BUILD.to_string();
        for _ in 0..3 {
            routing.record(&earned).await;
        }
        assert!(
            routing.qualifies(local.as_ref(), &build, build_class),
            "earned by record"
        );
        let mut failed = earned.clone();
        failed.verdict = Verdict::NotVerified;
        routing.record(&failed).await;
        assert!(
            !routing.qualifies(local.as_ref(), &build, build_class),
            "a failed verification puts it back on probation"
        );
        let record = registry.record_of("krabby", CAPABILITY_BUILD).unwrap();
        assert_eq!((record.verified_done, record.claimed_not_verified), (3, 1));
    }

    #[tokio::test]
    async fn a_default_tier_moved_in_the_store_takes_effect() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("db"), vec![7u8; 32]).unwrap();
        let local: Arc<dyn Worker> = Arc::new(W("krabby", WorkerKind::Local));
        let registry = Arc::new(WorkerRegistry::fixed(store.clone(), vec![local.clone()]));
        let routing = RecordRouting::new(registry);
        routing.load().await.unwrap();
        let seeded = store.workers().default_tiers().await.unwrap();
        assert_eq!(seeded.len(), 2, "the policy's prior is in the store");
        assert!(seeded.iter().all(|d| d.set_by == "policy"));
        let code = item(WorkKind::Code, vec![]);
        assert!(routing.qualifies(local.as_ref(), &code, Some(CODE)));

        // An accepted proposal moves code up; the controller only reads it.
        store
            .workers()
            .set_default_tier(CODE, 2, "item-routing-1", None)
            .await
            .unwrap();
        routing.reload().await.unwrap();
        assert_eq!(routing.default_tier(CODE), Some(2));
        assert!(!routing.qualifies(local.as_ref(), &code, Some(CODE)));
        routing.load().await.unwrap();
        assert_eq!(
            routing.default_tier(CODE),
            Some(2),
            "a restart keeps the move"
        );
    }
}
