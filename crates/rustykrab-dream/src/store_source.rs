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

    async fn verdict_totals(&self, ground_truth_only: bool) -> Result<OutcomeTally> {
        self.outcomes.verdict_totals(ground_truth_only).await
    }
}

/// Keeps completed analysis passes in the database.
#[derive(Clone)]
pub struct StoreReportSink {
    reports: rustykrab_store::DreamReportStore,
}

impl StoreReportSink {
    pub fn new(reports: rustykrab_store::DreamReportStore) -> Self {
        Self { reports }
    }
}

#[async_trait::async_trait]
impl crate::worker::ReportSink for StoreReportSink {
    async fn record_report(&self, report: &crate::report::AnalysisReport) -> Result<()> {
        // Serializing cannot fail for a report — every field is plain data
        // — but a serializer error must not be reported as a storage one,
        // so it is named for what it is.
        let json = serde_json::to_string(report).map_err(|e| {
            rustykrab_core::Error::Internal(format!("could not serialize analysis report: {e}"))
        })?;

        self.reports
            .record(
                report.generated_at,
                report.readiness.as_str(),
                report.total_records,
                &report.summary(),
                &json,
            )
            .await?;
        Ok(())
    }
}
