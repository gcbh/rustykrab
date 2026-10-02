//! Read-only analysis over recorded outcomes — the Analyze stage of the
//! self-improvement outer loop (see `DREAMING.md`).
//!
//! Everything here is a deterministic query. No model is called, nothing
//! is written, and no artifact is changed. The output is a report a human
//! reads to decide whether the signal is good enough to act on at all.
//!
//! That last question is the point. The design gates every later phase on
//! "reports show real, actionable patterns", so the most important thing
//! this stage produces is not a ranking of skills — it is an honest
//! account of whether the measurement is usable yet. A report that says
//! "not enough evidence" is a successful report.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use rustykrab_core::outcome::{AttributionKind, OutcomeTally};
use rustykrab_core::Result;

/// Minimum decisive observations before an artifact's success rate is
/// reported as meaningful rather than withheld.
///
/// Set deliberately above 1: the failure this guards against is reading a
/// confident-looking rate off a near-empty sample and acting on it.
pub const MIN_OBSERVATIONS: u32 = 5;

/// Success rate at or below which an artifact is called out as
/// underperforming, once it has enough evidence to judge.
pub const UNDERPERFORMING_BELOW: f64 = 0.5;

/// Where tallies are read from.
///
/// A trait rather than a concrete store so the analysis can be tested
/// against fabricated evidence without a database, and so a later stage
/// can feed it a filtered view.
#[async_trait::async_trait]
pub trait OutcomeSource: Send + Sync {
    /// Every artifact of `kind` that has at least one recorded outcome.
    async fn tallies(
        &self,
        kind: AttributionKind,
        ground_truth_only: bool,
    ) -> Result<Vec<(String, OutcomeTally)>>;

    /// Total records held, regardless of attribution.
    async fn total_records(&self) -> Result<u32>;

    /// Verdict totals over the records themselves, counting each record
    /// once.
    ///
    /// Separate from [`Self::tallies`] because the two answer different
    /// questions. A tally describes an *artifact* and is necessarily
    /// reached through the attribution join. Signal quality describes the
    /// *record population* — how much evidence exists and how much of it
    /// is ground truth — and asking that through the join makes the answer
    /// depend on how many artifacts happened to be in play, which is a
    /// property of the traffic rather than of the evidence.
    async fn verdict_totals(&self, ground_truth_only: bool) -> Result<OutcomeTally>;
}

/// What the evidence supports saying about one artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingVerdict {
    /// Enough evidence, and it looks fine.
    Healthy,
    /// Enough evidence, and it is failing more often than not.
    Underperforming,
    /// Not enough decisive observations to say either way. Reported rather
    /// than hidden: "we cannot tell yet" is a finding about coverage.
    InsufficientEvidence,
}

/// One artifact and what its record says about it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactFinding {
    pub id: String,
    pub tally: OutcomeTally,
    /// `None` below [`MIN_OBSERVATIONS`].
    pub success_rate: Option<f64>,
    pub verdict: FindingVerdict,
}

impl ArtifactFinding {
    fn new(id: String, tally: OutcomeTally) -> Self {
        let success_rate = tally.success_rate(MIN_OBSERVATIONS);
        let verdict = match success_rate {
            None => FindingVerdict::InsufficientEvidence,
            Some(rate) if rate <= UNDERPERFORMING_BELOW => FindingVerdict::Underperforming,
            Some(_) => FindingVerdict::Healthy,
        };
        Self {
            id,
            tally,
            success_rate,
            verdict,
        }
    }
}

/// How trustworthy the collected signal is, independent of what it says.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SignalQuality {
    /// Records that landed on success or failure.
    pub decisive: u32,
    /// Records the evidence could not call either way.
    pub ambiguous: u32,
    /// Decisive records resting on verifiable or explicit evidence.
    pub ground_truth: u32,
    /// Share of all records that were ambiguous.
    pub ambiguous_rate: f64,
    /// Share of decisive records backed by ground truth rather than a proxy.
    pub ground_truth_rate: f64,
}

impl SignalQuality {
    fn from_totals(all: &OutcomeTally, ground_truth: &OutcomeTally) -> Self {
        let decisive = all.decisive();
        let total = decisive + all.ambiguous;
        Self {
            decisive,
            ambiguous: all.ambiguous,
            ground_truth: ground_truth.decisive(),
            ambiguous_rate: if total == 0 {
                0.0
            } else {
                all.ambiguous as f64 / total as f64
            },
            ground_truth_rate: if decisive == 0 {
                0.0
            } else {
                ground_truth.decisive() as f64 / decisive as f64
            },
        }
    }
}

/// Whether the evidence is strong enough to justify letting a later stage
/// change anything.
///
/// This is the phase gate from `DREAMING.md` expressed in code rather than
/// left to judgement, and it is deliberately hard to satisfy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Readiness {
    /// Too few records to conclude anything.
    InsufficientData,
    /// Records exist, but all of them rest on proxy signals. Acting on
    /// this would be optimizing the measurement rather than the system.
    ProxyOnly,
    /// Ground-truth evidence exists in usable quantity.
    Ready,
}

impl Readiness {
    /// Whether a mutating stage may proceed on this evidence.
    pub fn permits_mutation(&self) -> bool {
        matches!(self, Self::Ready)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::InsufficientData => "insufficient_data",
            Self::ProxyOnly => "proxy_only",
            Self::Ready => "ready",
        }
    }
}

/// The result of one analysis pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisReport {
    pub generated_at: DateTime<Utc>,
    pub total_records: u32,
    pub signal_quality: SignalQuality,
    pub readiness: Readiness,
    pub skills: Vec<ArtifactFinding>,
    pub memories: Vec<ArtifactFinding>,
    pub tools: Vec<ArtifactFinding>,
}

impl AnalysisReport {
    /// Artifacts of every kind that have enough evidence to be called
    /// underperforming — the only findings a later stage should act on.
    pub fn underperforming(&self) -> Vec<(AttributionKind, &ArtifactFinding)> {
        let mut out = Vec::new();
        for (kind, list) in [
            (AttributionKind::Skill, &self.skills),
            (AttributionKind::Memory, &self.memories),
            (AttributionKind::Tool, &self.tools),
        ] {
            for finding in list
                .iter()
                .filter(|f| f.verdict == FindingVerdict::Underperforming)
            {
                out.push((kind, finding));
            }
        }
        out
    }

    /// A short human-readable digest.
    ///
    /// The readiness verdict leads, because whether the evidence can be
    /// trusted matters more than what it happens to say.
    pub fn summary(&self) -> String {
        let mut lines = vec![format!(
            "Outcome analysis: {} records, readiness {}",
            self.total_records,
            self.readiness.as_str()
        )];

        lines.push(format!(
            "  signal: {} decisive, {} ambiguous ({:.0}% ambiguous), {:.0}% ground truth",
            self.signal_quality.decisive,
            self.signal_quality.ambiguous,
            self.signal_quality.ambiguous_rate * 100.0,
            self.signal_quality.ground_truth_rate * 100.0,
        ));

        match self.readiness {
            Readiness::InsufficientData => {
                lines.push("  not enough evidence to draw conclusions yet".to_string());
            }
            Readiness::ProxyOnly => {
                lines.push(
                    "  all evidence rests on proxy signals; findings are indicative only"
                        .to_string(),
                );
            }
            Readiness::Ready => {}
        }

        let underperforming = self.underperforming();
        if underperforming.is_empty() {
            lines.push("  no artifact has enough evidence to be called underperforming".into());
        } else {
            lines.push(format!("  {} underperforming:", underperforming.len()));
            for (kind, finding) in underperforming {
                lines.push(format!(
                    "    {} {}: {}/{} succeeded",
                    kind.as_str(),
                    finding.id,
                    finding.tally.helpful,
                    finding.tally.decisive(),
                ));
            }
        }

        lines.join("\n")
    }
}

/// Run one read-only analysis pass.
///
/// Six aggregate queries and no writes. Safe to run at any time; it is
/// confined to downtime only because it competes for the database.
pub async fn analyze(source: &dyn OutcomeSource) -> Result<AnalysisReport> {
    let total_records = source.total_records().await?;

    // Signal quality is measured over the records, not through the
    // attribution join.
    //
    // Measuring it through attributions requires picking one kind to
    // represent the population, because every kind draws from the same
    // records and summing across them counts a record once per artifact it
    // touched. Any such choice is wrong: keying on skills makes the
    // readiness of the whole loop depend on how often a skill happened to
    // be active, so a deployment that runs few skills reports
    // `InsufficientData` next to a five-figure record count, forever. The
    // records themselves have no such bias.
    let all = source.verdict_totals(false).await?;
    let ground_truth = source.verdict_totals(true).await?;

    let mut per_kind = Vec::new();
    for kind in [
        AttributionKind::Skill,
        AttributionKind::Memory,
        AttributionKind::Tool,
    ] {
        let tallies = source.tallies(kind, false).await?;
        per_kind.push(
            tallies
                .into_iter()
                .map(|(id, tally)| ArtifactFinding::new(id, tally))
                .collect::<Vec<_>>(),
        );
    }

    let signal_quality = SignalQuality::from_totals(&all, &ground_truth);
    let readiness = if all.decisive() < MIN_OBSERVATIONS {
        Readiness::InsufficientData
    } else if ground_truth.decisive() == 0 {
        Readiness::ProxyOnly
    } else {
        Readiness::Ready
    };

    let mut kinds = per_kind.into_iter();
    Ok(AnalysisReport {
        generated_at: Utc::now(),
        total_records,
        signal_quality,
        readiness,
        skills: kinds.next().unwrap_or_default(),
        memories: kinds.next().unwrap_or_default(),
        tools: kinds.next().unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Fabricated evidence, so the analysis can be exercised without a
    /// database and with exactly the distributions a case needs.
    ///
    /// Artifact tallies and record totals are set independently, because
    /// in the real store they come from different queries and can
    /// legitimately disagree — a record with no attributions counts toward
    /// the population but toward no artifact. A fake that derived one from
    /// the other could not express the case that motivated the split.
    #[derive(Default)]
    struct FakeSource {
        all: HashMap<&'static str, Vec<(String, OutcomeTally)>>,
        records: OutcomeTally,
        gt_records: OutcomeTally,
    }

    fn tally(helpful: u32, harmful: u32, ambiguous: u32) -> OutcomeTally {
        OutcomeTally {
            helpful,
            harmful,
            ambiguous,
        }
    }

    impl FakeSource {
        fn with(mut self, kind: AttributionKind, rows: Vec<(&str, OutcomeTally)>) -> Self {
            let rows: Vec<(String, OutcomeTally)> =
                rows.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
            self.all.insert(kind.as_str(), rows);
            self
        }

        /// The record population, independent of what it was attributed to.
        fn with_records(mut self, all: OutcomeTally, ground_truth: OutcomeTally) -> Self {
            self.records = all;
            self.gt_records = ground_truth;
            self
        }
    }

    #[async_trait::async_trait]
    impl OutcomeSource for FakeSource {
        async fn tallies(
            &self,
            kind: AttributionKind,
            _ground_truth_only: bool,
        ) -> Result<Vec<(String, OutcomeTally)>> {
            Ok(self.all.get(kind.as_str()).cloned().unwrap_or_default())
        }

        async fn total_records(&self) -> Result<u32> {
            Ok(self.records.decisive() + self.records.ambiguous)
        }

        async fn verdict_totals(&self, ground_truth_only: bool) -> Result<OutcomeTally> {
            Ok(if ground_truth_only {
                self.gt_records.clone()
            } else {
                self.records.clone()
            })
        }
    }

    #[tokio::test]
    async fn empty_evidence_reports_insufficient_rather_than_healthy() {
        // The dangerous failure is a clean-looking report over no data.
        let report = analyze(&FakeSource::default()).await.unwrap();
        assert_eq!(report.readiness, Readiness::InsufficientData);
        assert!(!report.readiness.permits_mutation());
        assert_eq!(report.total_records, 0);
        assert!(report.underperforming().is_empty());
    }

    #[tokio::test]
    async fn thin_evidence_withholds_a_success_rate() {
        // One failure out of one is not a 0% success rate.
        let source = FakeSource::default()
            .with(AttributionKind::Skill, vec![("thin", tally(0, 1, 0))])
            .with_records(tally(0, 1, 0), tally(0, 0, 0));

        let report = analyze(&source).await.unwrap();
        let finding = &report.skills[0];
        assert_eq!(finding.success_rate, None);
        assert_eq!(finding.verdict, FindingVerdict::InsufficientEvidence);
    }

    #[tokio::test]
    async fn proxy_only_evidence_does_not_permit_mutation() {
        // Plenty of records, none of them ground truth. Acting here would
        // optimize the measurement rather than the system.
        let source = FakeSource::default()
            .with(AttributionKind::Skill, vec![("proxied", tally(8, 4, 2))])
            .with_records(tally(8, 4, 2), tally(0, 0, 0));

        let report = analyze(&source).await.unwrap();
        assert_eq!(report.readiness, Readiness::ProxyOnly);
        assert!(!report.readiness.permits_mutation());
        assert!(report.summary().contains("proxy signals"));
    }

    #[tokio::test]
    async fn ground_truth_evidence_permits_mutation() {
        let source = FakeSource::default()
            .with(AttributionKind::Skill, vec![("solid", tally(9, 3, 1))])
            .with_records(tally(9, 3, 1), tally(6, 2, 0));

        let report = analyze(&source).await.unwrap();
        assert_eq!(report.readiness, Readiness::Ready);
        assert!(report.readiness.permits_mutation());
        assert!(report.signal_quality.ground_truth_rate > 0.0);
    }

    #[tokio::test]
    async fn readiness_does_not_depend_on_whether_skills_were_active() {
        // The bug this guards: signal quality was summed over skill
        // attributions alone, so a deployment that runs few skills — or
        // none — reported InsufficientData no matter how many records it
        // had, and the phase gate could never be evaluated.
        //
        // Same record population both times; the only difference is
        // whether a skill happened to be in play.
        let records = tally(20, 5, 3);
        let ground_truth = tally(12, 2, 0);

        let with_skills = FakeSource::default()
            .with(AttributionKind::Skill, vec![("s", tally(20, 5, 3))])
            .with_records(records.clone(), ground_truth.clone());

        let without_skills = FakeSource::default()
            .with(AttributionKind::Tool, vec![("browser", tally(20, 5, 3))])
            .with_records(records, ground_truth);

        let a = analyze(&with_skills).await.unwrap();
        let b = analyze(&without_skills).await.unwrap();

        assert_eq!(a.readiness, Readiness::Ready);
        assert_eq!(
            b.readiness,
            Readiness::Ready,
            "readiness must describe the evidence, not the traffic mix"
        );
        assert_eq!(a.signal_quality.decisive, b.signal_quality.decisive);
        assert_eq!(a.signal_quality.ambiguous, b.signal_quality.ambiguous);
        assert_eq!(a.total_records, b.total_records);
    }

    #[tokio::test]
    async fn signal_quality_counts_a_record_once_however_it_was_attributed() {
        // One record can be attributed to a skill, several memories and
        // several tools. Reading the population off the attribution join
        // counts that record many times and overstates the evidence.
        let source = FakeSource::default()
            .with(AttributionKind::Skill, vec![("s", tally(10, 0, 0))])
            .with(AttributionKind::Memory, vec![("m", tally(10, 0, 0))])
            .with(AttributionKind::Tool, vec![("t", tally(10, 0, 0))])
            .with_records(tally(10, 0, 0), tally(10, 0, 0));

        let report = analyze(&source).await.unwrap();
        assert_eq!(
            report.signal_quality.decisive, 10,
            "evidence must be counted once, not once per attributed artifact"
        );
        assert_eq!(report.total_records, 10);
    }

    #[tokio::test]
    async fn well_evidenced_failure_is_flagged() {
        let source = FakeSource::default()
            .with(
                AttributionKind::Skill,
                vec![("bad", tally(1, 9, 0)), ("good", tally(9, 1, 0))],
            )
            .with_records(tally(10, 10, 0), tally(10, 10, 0));

        let report = analyze(&source).await.unwrap();
        let flagged = report.underperforming();

        assert_eq!(flagged.len(), 1);
        assert_eq!(flagged[0].1.id, "bad");
        assert_eq!(flagged[0].0, AttributionKind::Skill);

        let good = report.skills.iter().find(|f| f.id == "good").unwrap();
        assert_eq!(good.verdict, FindingVerdict::Healthy);
    }

    #[tokio::test]
    async fn ambiguity_is_reported_not_hidden() {
        // A high ambiguous rate is a finding about the signal itself, and
        // ambiguous records must not be counted toward either verdict.
        let source = FakeSource::default()
            .with(AttributionKind::Skill, vec![("murky", tally(3, 3, 30))])
            .with_records(tally(3, 3, 30), tally(3, 3, 0));

        let report = analyze(&source).await.unwrap();
        assert_eq!(report.signal_quality.ambiguous, 30);
        assert_eq!(report.signal_quality.decisive, 6);
        assert!(report.signal_quality.ambiguous_rate > 0.8);
        assert!(report.summary().contains("ambiguous"));
    }

    #[tokio::test]
    async fn every_kind_is_analyzed() {
        let source = FakeSource::default()
            .with(AttributionKind::Skill, vec![("s", tally(6, 0, 0))])
            .with(AttributionKind::Memory, vec![("m", tally(6, 0, 0))])
            .with(AttributionKind::Tool, vec![("t", tally(6, 0, 0))])
            .with_records(tally(6, 0, 0), tally(0, 0, 0));

        let report = analyze(&source).await.unwrap();
        assert_eq!(report.skills.len(), 1);
        assert_eq!(report.memories.len(), 1);
        assert_eq!(report.tools.len(), 1);
    }
}
