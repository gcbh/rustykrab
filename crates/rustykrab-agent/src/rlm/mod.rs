//! Recursive Language Model (RLM) pattern.
//!
//! Implements the REPL-style RLM scaffold from the foundational paper
//! (Zhang, Kraska, Khattab — arXiv 2512.24601):
//!
//! - Context is stored as an external variable, NOT in the prompt
//! - The model explores context via tools: peek, search, sub_query
//! - Sub-queries recurse on model-selected context slices
//! - No single call handles the full context — the tool layer manages it

pub mod context_manager;
pub mod context_store;
pub mod recursive_call;
pub mod repl_tools;

pub use context_manager::estimate_tokens;
pub use context_store::ContextStore;
pub use recursive_call::RecursiveExecutor;
pub use repl_tools::context_tools;
