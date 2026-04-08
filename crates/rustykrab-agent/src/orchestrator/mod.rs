//! Recursive agentic orchestration layer.
//!
//! Implements the Decompose → Execute → Synthesize → Verify pipeline
//! that maximizes effective intelligence of smaller local models by
//! ensuring no single LLM call handles too much context.

pub mod decomposer;
pub mod executor;
pub mod pipeline;
pub mod refiner;
pub mod synthesizer;
pub mod verifier;

pub use decomposer::Decomposer;
pub use executor::ParallelExecutor;
pub use pipeline::{OrchestrationPipeline, PipelineResult};
pub use refiner::RefinementLoop;
pub use synthesizer::Synthesizer;
pub use verifier::ConsistencyVoter;
