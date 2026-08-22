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
//! See `DREAMING.md` for the full design. This crate currently implements
//! the **Analyze** stage only: deterministic, read-only reporting over
//! recorded outcomes. Nothing here calls a model or changes any artifact.

pub mod engine;
pub mod memory_mutator;
pub mod mutation;
pub mod report;
pub mod store_source;
pub mod worker;

pub use engine::{promote, rollback, rollback_blockers, CyclePolicy, Promotion, PromotionRefusal};
pub use memory_mutator::MemorySystemMutator;
pub use mutation::{MemoryFacts, MemoryMutator};
pub use report::{
    analyze, AnalysisReport, ArtifactFinding, FindingVerdict, OutcomeSource, Readiness,
    SignalQuality, MIN_OBSERVATIONS, UNDERPERFORMING_BELOW,
};
pub use store_source::StoreOutcomeSource;
pub use worker::{DreamWorker, PassOutcome, WorkerConfig};
