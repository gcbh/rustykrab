//! The self-improvement outer loop for RustyKrab.
//!
//! RustyKrab runs at two timescales. The **inner loop** is `AgentRunner`:
//! one turn, seconds to minutes, with a policy fixed to whatever memory and
//! skills currently exist. The **outer loop** — this crate — observes many
//! inner-loop executions and adjusts the durable state that parameterizes
//! the inner loop, so that future turns go better.
//!
//! The two are closed over each other: outer-loop outputs become inner-loop
//! inputs, and inner-loop traces become the outer loop's measurement.
//!
//! See `DREAMING.md` for the full design. This crate implements the
//! **Analyze** stage (deterministic, read-only reporting over recorded
//! outcomes) and the **Plan + Execute** stage for memory: a stage-then-
//! promote consolidation cycle with a manifest that makes every promoted
//! change reversible. Nothing here calls a model. The `eval` module is the
//! protocol the crate's own evals report through.

pub mod cluster_source;
pub mod consolidation;
pub mod engine;
pub mod eval;
pub mod memory_mutator;
pub mod mutation;
pub mod planner;
pub mod report;
pub mod store_source;
pub mod worker;

pub use cluster_source::MemoryClusterSource;
pub use consolidation::{run_consolidation_cycle, ConsolidationContext, CycleOutcome};
pub use engine::{promote, rollback, rollback_blockers, CyclePolicy, Promotion, PromotionRefusal};
pub use memory_mutator::MemorySystemMutator;
pub use mutation::{MemoryFacts, MemoryMutator};
pub use planner::{plan_consolidation, ConsolidationPlan, ConsolidationSource, MemoryCandidate};
pub use report::{
    analyze, AnalysisReport, ArtifactFinding, FindingVerdict, OutcomeSource, Readiness,
    SignalQuality, MIN_OBSERVATIONS, UNDERPERFORMING_BELOW,
};
pub use store_source::{StoreOutcomeSource, StoreReportSink};
pub use worker::{DreamWorker, PassOutcome, ReportSink, WorkerConfig};
