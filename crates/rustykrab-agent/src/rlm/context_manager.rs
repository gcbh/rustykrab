//! Token estimation for recursive calls.
//!
//! Re-exported from `rustykrab-core` so recursive sub-calls size context the
//! same way compaction does. A local copy of the heuristic here would let the
//! RLM executor and the runner disagree about whether a slice fits.

pub use rustykrab_core::estimate_text_tokens as estimate_tokens;
