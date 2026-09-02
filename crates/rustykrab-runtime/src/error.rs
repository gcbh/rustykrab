//! What can go wrong assembling or running a turn.
//!
//! Deliberately not an HTTP status. This layer previously returned
//! `StatusCode` — every failure was `INTERNAL_SERVER_ERROR` — which meant a
//! Telegram loop received a web status code for a problem that had nothing
//! to do with the web, and the only transport that could act on it was the
//! one that never needed to.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum RuntimeError {
    /// The agent loop failed, or the turn could not be assembled.
    #[error("agent run failed")]
    Internal,
}
