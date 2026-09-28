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
//! - [`handle`]: the controller as the gateway, the CLI and the tools see it.
//! - [`import`]: the delivery import, a `StackManifest` turned into the
//!   layered `code` graph of section 14.1, filed through the validator.
//! - [`worker`]: workers as the controller sees them, and the brief they get.
//! - [`registry`]: the named workers (section 5), persisted in the store's
//!   `workers` table, and the factory that builds external ones and peers.
//! - [`peer`]: the delegation contract a peer worker and its node share
//!   (Phase 5): the typed submission, the task view with its typed result,
//!   the ceiling refusal, the node's advertisement, and the node side's
//!   `NodeWorkers` seam.
//! - [`routing`]: work classes, the routing record and the policy that reads
//!   it (sections 5 and 10).
//! - [`workspace`]: isolated git worktrees for `code` runs and the check of a
//!   `code` result against them (section 5).
//! - [`review`]: the review surface of section 11: which items are projected
//!   to issues and what they say, the adapter trait, and the one-way sync
//!   that brings decisions back as typed events.
//! - [`controller`]: the loop itself (section 6), which drives the store
//!   through the pure modules above.
//!
//! The shared data types live in `rustykrab_core::work`, so the store, the
//! tools and the CLI agree on the vocabulary without depending on this crate.

pub mod controller;
pub mod errors;
pub mod graph;
pub mod handle;
pub mod import;
pub mod ladder;
pub mod peer;
pub mod registry;
pub mod review;
pub mod routing;
pub mod worker;
pub mod workspace;

/// Who filed and on whose behalf: the record every filing path through
/// [`handle::ControlHandle`] takes. Defined beside the work tools'
/// backend in `rustykrab-tools`; re-exported so a caller of the handle
/// (the gateway) needs no dependency on the tool crate for it.
pub use rustykrab_tools::work_backend::Provenance;
