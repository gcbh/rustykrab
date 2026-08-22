//! Reads analysis evidence out of the SQLite outcome store.

use rustykrab_core::outcome::{AttributionKind, OutcomeTally};
use rustykrab_core::Result;
use rustykrab_store::OutcomeStore;

use crate::report::OutcomeSource;

/// Backs [`OutcomeSource`] with the persisted outcome records.
#[derive(Clone)]
pub struct StoreOutcomeSource {
    outcomes: OutcomeStore,
}

impl StoreOutcomeSource {
    pub fn new(outcomes: OutcomeStore) -> Self {
        Self { outcomes }
    }
}

#[async_trait::async_trait]
impl OutcomeSource for StoreOutcomeSource {
    async fn tallies(
        &self,
        kind: AttributionKind,
        ground_truth_only: bool,
    ) -> Result<Vec<(String, OutcomeTally)>> {
        self.outcomes.tallies_by_kind(kind, ground_truth_only).await
    }

    async fn total_records(&self) -> Result<u32> {
        self.outcomes.count().await
    }
}
