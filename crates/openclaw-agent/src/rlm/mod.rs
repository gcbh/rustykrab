//! Recursive Language Model (RLM) pattern.
//!
//! Implements the RLM scaffold from Prime Intellect's research:
//! - The user's request is treated as a variable processed programmatically
//! - The model can request sub-LLM calls (recursive delegation)
//! - Each sub-call gets its own focused context window
//! - Results are collected and fed back to the parent call
//! - No single call handles the full context — the Rust layer manages the tree

pub mod context_manager;
pub mod recursive_call;

pub use context_manager::ContextManager;
pub use recursive_call::RecursiveExecutor;
