//! Re-export of the memory surface the `memory_*` tools call.
//!
//! The trait itself lives in `rustykrab-core` so `rustykrab-memory` — which
//! sits below this crate — can implement it directly. Re-exported here so
//! `rustykrab_tools::MemoryBackend` keeps resolving for existing callers.
pub use rustykrab_core::memory_backend::MemoryBackend;
