//! The review surface (plan section 11): the store is the source of truth
//! and issues are where a human reviews engineering work.
//!
//! - [`project`] is the projection rule and the projected fields, pure:
//!   which kinds become issues (`code`, `proposal`, `internal`, and a
//!   `capability` build; never `personal`, `research` or a capability
//!   acquisition or request), what an issue says (title, objective,
//!   `done_when`, status or roll-up, worker, evidence, parent and edges as
//!   references), and how a local-only neighbour appears: as the opaque
//!   `local:#N`, never its title, objective or evidence.
//! - [`ReviewSurface`] is the adapter a surface implements. GitHub issues
//!   is the first (`rustykrab-cli`); Linear can follow behind the same
//!   trait, since the review-surface decision is still open.
//! - [`sync`] is one pass, one way: decisions taken on the issue (the
//!   decision labels and approval comments of [`DecisionVocabulary`]) come
//!   back first as typed review events through
//!   [`crate::handle::ControlHandle::review_decision`], then every
//!   projectable item is written out, and a hand edit to a projected field
//!   is overwritten because the issue no longer matches its projection.
//!   Nothing else flows back, and the issue never decides readiness.

mod project;
mod sync;

use async_trait::async_trait;
use rustykrab_core::Error;
use serde::{Deserialize, Serialize};

pub use project::{
    digest, is_projectable, local_ref, project, ItemView, Projection, ProjectionContext,
    LABEL_ACCEPTED, LABEL_DECLINED, LABEL_MANAGED,
};
pub use sync::{parse_decisions, pull_decisions, push_projections, sync, Synced};

/// One issue as the surface holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    /// The surface's id for it: a GitHub issue number.
    pub number: String,
    pub url: Option<String>,
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub open: bool,
}

/// One comment on an issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comment {
    /// The surface's id, increasing with time on GitHub.
    pub id: String,
    pub author: String,
    /// Whether the author may decide on the repository's behalf (on GitHub
    /// an owner, member or collaborator). Anyone else's command is ignored.
    pub trusted: bool,
    pub body: String,
}

/// The labels and comment commands a human decides with (section 11: the
/// decision vocabulary is the only way to act from the issue).
#[derive(Debug, Clone, Copy, Default)]
pub struct DecisionVocabulary;

impl DecisionVocabulary {
    /// The comment commands, each at the start of a comment's first line.
    pub const ACCEPT: &'static str = "/accept";
    pub const DECLINE: &'static str = "/decline";
    pub const AMEND: &'static str = "/amend";
}

/// A review surface an item can be projected to. Every call is one
/// request; a failed one leaves the store unchanged and the next pass
/// retries it.
#[async_trait]
pub trait ReviewSurface: Send + Sync {
    /// A short stable name (`github`), recorded with each projection.
    fn name(&self) -> &str;

    /// Every issue the surface holds under [`LABEL_MANAGED`], open or
    /// closed.
    async fn issues(&self) -> Result<Vec<Issue>, Error>;

    /// One issue by number, for one [`ReviewSurface::issues`] missed (its
    /// managed label removed by hand). `None` when it no longer exists.
    async fn issue(&self, number: &str) -> Result<Option<Issue>, Error>;

    /// Open an issue with the projected fields and labels.
    async fn create(&self, projection: &Projection) -> Result<Issue, Error>;

    /// Rewrite an issue's projected fields (title, body, open or closed)
    /// and add any of the projection's labels it lacks, leaving every
    /// other label (a human's decision) where it is.
    async fn update(&self, number: &str, projection: &Projection) -> Result<Issue, Error>;

    /// Comments on an issue, oldest first.
    async fn comments(&self, number: &str) -> Result<Vec<Comment>, Error>;
}
