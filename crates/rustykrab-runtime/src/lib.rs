//! Running an agent turn, independent of how the turn was asked for.
//!
//! The gateway crate used to own this. It builds the system prompt, derives
//! the session's capability set, creates the `AgentRunner`, installs the
//! memory write-back hook and drives the loop — none of which is HTTP. The
//! consequence was that `rustykrab-cli`'s Telegram and Slack loops depended
//! on an Axum server crate to do non-HTTP work, and anything wanting to
//! embed the agent inherited tower, APNs, rate limiting and origin policy
//! with it.
//!
//! [`AgentContext`] is the state a turn needs; the gateway now holds one
//! alongside its own HTTP-only fields.

mod context;
mod distill;
mod error;
mod orchestrate;

pub use context::AgentContext;
pub use distill::{distil_into_memory, ingest_inbound};
pub use error::RuntimeError;
pub use orchestrate::{
    run_agent, run_agent_interactive, run_agent_streaming, run_agent_streaming_with_options,
    run_agent_with_options, RunOptions,
};
