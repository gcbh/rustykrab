//! RustyKrab's control layer (`docs/plans/control-layer-and-worker-fleet.md`).
//!
//! The controller is a deterministic loop in code, never a model. It
//! schedules a graph of work items joined by typed edges and parent links,
//! computes readiness, cascade and roll-up, climbs the resolution ladder on
//! every failure, and surfaces to the user only when the ladder is spent.
//!
//! Layout:
//!
//! - [`graph`]: pure functions over an in-memory snapshot of items and edges:
//!   filing validation (section 14.1), readiness (4.1, 4.2), cascade (4.5),
//!   roll-up (4.2), re-pointing (4.4) and aging candidates (4.6). No I/O.
//! - [`errors`]: the error taxonomy and deterministic classifiers (section 9).
//! - [`ladder`]: the resolution ladder as a pure state machine over an item's
//!   rung history and budgets (sections 8 and 6.4).
//! - [`controller`]: the loop itself (section 6), which drives the store
//!   through the pure modules above.
//!
//! The shared data types live in `rustykrab_core::work`, so the store, the
//! tools and the CLI agree on the vocabulary without depending on this crate.

pub mod controller;
pub mod errors;
pub mod graph;
pub mod ladder;
