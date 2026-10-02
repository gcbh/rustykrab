//! Checking that a run actually had the effect it was supposed to have.
//!
//! A skill's `[outcome]` block names the effects a successful run
//! produces. Something has to go and look, or the declaration is a
//! comment. This is the looking.
//!
//! ## Why not "a tool of that name succeeded"
//!
//! The obvious cheap implementation is to treat a check as satisfied when
//! the run called a tool by that name and the call returned Ok. It is
//! cheap because the tracer already has the data, and it is wrong because
//! **the agent controls it**. "Did the agent call `calendar_create_event`"
//! is a question about the agent's behaviour, not about the world; a run
//! that called the tool with the wrong date, the wrong attendee, or the
//! wrong calendar satisfies it exactly as well as a correct one.
//!
//! That would not matter if the answer went nowhere. It goes to
//! [`SignalClass::Verifiable`], which is what `is_ground_truth()` returns
//! true for, which is what `Readiness::Ready` requires, which is what
//! permits the outer loop to mutate memory. So a proxy the agent controls
//! would be the sole evidence authorizing self-modification — the loop
//! grading its own homework, and precisely the Goodhart failure
//! `DREAMING.md` names as the thing to avoid.
//!
//! ## What a probe is instead
//!
//! A probe observes **state**, not behaviour, through a path independent
//! of the run: it queries the calendar, it stats the file. And it is
//! sampled twice — once before the run and once after — because "the
//! effect holds" is a much weaker claim than "this run produced it". A
//! calendar that already contained the event proves nothing about the turn
//! that just finished.
//!
//! A check therefore passes only when the observation **changed to
//! holding**. That is a post-condition; the tool-call version was not.
//!
//! ## The unknown-check rule
//!
//! A check naming no registered probe yields **no contract at all**, so
//! the run falls back to the implicit signal. It does not silently pass
//! (which would invent ground truth) and it does not silently fail (which
//! would manufacture failures for a working skill, and is what a typo in a
//! `SKILL.md` would otherwise do forever).

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::Result;

/// What a probe saw.
///
/// `None` means the effect does not hold. `Some(fingerprint)` means it
/// does, where the fingerprint distinguishes one holding state from
/// another — an event id, a file's size and mtime, a row count. Two
/// samples that both hold but differ mean the effect was *re*-produced,
/// which counts.
pub type Observation = Option<String>;

/// A named effect that can be observed independently of the run.
#[async_trait::async_trait]
pub trait PostCondition: Send + Sync {
    /// The name a `SKILL.md` check refers to.
    fn name(&self) -> &str;

    /// Observe the effect as it stands right now.
    ///
    /// Must not consult the agent's trace, its tool calls, or anything the
    /// run reported about itself. If the only way to answer is to ask the
    /// agent what it did, this is not a post-condition and should not be
    /// registered as one.
    async fn observe(&self) -> Result<Observation>;
}

/// The probes a deployment knows how to run.
///
/// Built by whoever wires the daemon, because probes need the same
/// backends the tools use, and passed to the runner. Empty by default:
/// a deployment that has registered nothing can produce no ground truth,
/// which is the honest state rather than a degraded one.
#[derive(Default, Clone)]
pub struct ProbeRegistry {
    probes: BTreeMap<String, Arc<dyn PostCondition>>,
}

impl ProbeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, probe: Arc<dyn PostCondition>) -> Self {
        self.probes.insert(probe.name().to_string(), probe);
        self
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn PostCondition>> {
        self.probes.get(name)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.probes.contains_key(name)
    }

    pub fn is_empty(&self) -> bool {
        self.probes.is_empty()
    }

    pub fn names(&self) -> Vec<&str> {
        self.probes.keys().map(|s| s.as_str()).collect()
    }

    /// Sample every named check.
    ///
    /// Unknown names are absent from the result rather than recorded as
    /// not-holding, so the caller can tell "the effect is not there" from
    /// "nobody knows how to look".
    pub async fn sample(&self, checks: &[String]) -> BTreeMap<String, Observation> {
        let mut out = BTreeMap::new();
        for check in checks {
            let Some(probe) = self.probes.get(check) else {
                continue;
            };
            match probe.observe().await {
                Ok(observation) => {
                    out.insert(check.clone(), observation);
                }
                Err(_e) => {
                    // A probe that cannot answer must not be read as
                    // "the effect is absent" -- that would score a working
                    // skill as having done nothing because a calendar
                    // server was briefly down. Omitting it downgrades the
                    // whole contract to the implicit signal instead.
                }
            }
        }
        out
    }
}

impl std::fmt::Debug for ProbeRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProbeRegistry")
            .field("probes", &self.names())
            .finish()
    }
}

/// A pair of samples taken either side of a run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProbeWindow {
    pub before: BTreeMap<String, Observation>,
    pub after: BTreeMap<String, Observation>,
}

impl ProbeWindow {
    /// Whether `check` went from not-holding (or holding differently) to
    /// holding across the window.
    ///
    /// Returns `None` when either sample is missing: an unobserved check
    /// is not a failed one.
    pub fn produced(&self, check: &str) -> Option<bool> {
        let before = self.before.get(check)?;
        let after = self.after.get(check)?;
        Some(after.is_some() && before != after)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Scripted {
        name: &'static str,
        observations: Mutex<Vec<Observation>>,
    }

    impl Scripted {
        fn new(name: &'static str, observations: Vec<Observation>) -> Arc<Self> {
            Arc::new(Self {
                name,
                observations: Mutex::new(observations),
            })
        }
    }

    #[async_trait::async_trait]
    impl PostCondition for Scripted {
        fn name(&self) -> &str {
            self.name
        }
        async fn observe(&self) -> Result<Observation> {
            let mut o = self.observations.lock().unwrap();
            Ok(if o.is_empty() { None } else { o.remove(0) })
        }
    }

    struct Broken;

    #[async_trait::async_trait]
    impl PostCondition for Broken {
        fn name(&self) -> &str {
            "broken"
        }
        async fn observe(&self) -> Result<Observation> {
            Err(crate::Error::Internal("calendar unreachable".into()))
        }
    }

    fn checks(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[tokio::test]
    async fn an_effect_that_appears_across_the_window_was_produced() {
        let registry =
            ProbeRegistry::new().with(Scripted::new("event", vec![None, Some("ev-1".into())]));
        let window = ProbeWindow {
            before: registry.sample(&checks(&["event"])).await,
            after: registry.sample(&checks(&["event"])).await,
        };
        assert_eq!(window.produced("event"), Some(true));
    }

    #[tokio::test]
    async fn an_effect_that_was_already_there_was_not_produced() {
        // The reason a probe is sampled twice. "The calendar contains the
        // event" says nothing about the turn that just ran if it contained
        // it beforehand, and scoring that as a success would credit every
        // subsequent turn for work done once.
        let registry = ProbeRegistry::new().with(Scripted::new(
            "event",
            vec![Some("ev-1".into()), Some("ev-1".into())],
        ));
        let window = ProbeWindow {
            before: registry.sample(&checks(&["event"])).await,
            after: registry.sample(&checks(&["event"])).await,
        };
        assert_eq!(window.produced("event"), Some(false));
    }

    #[tokio::test]
    async fn a_changed_effect_counts_as_produced() {
        // A second event, a rewritten file. The effect held before and
        // holds now, but it is not the same one.
        let registry = ProbeRegistry::new().with(Scripted::new(
            "event",
            vec![Some("ev-1".into()), Some("ev-2".into())],
        ));
        let window = ProbeWindow {
            before: registry.sample(&checks(&["event"])).await,
            after: registry.sample(&checks(&["event"])).await,
        };
        assert_eq!(window.produced("event"), Some(true));
    }

    #[tokio::test]
    async fn an_effect_that_went_away_was_not_produced() {
        let registry =
            ProbeRegistry::new().with(Scripted::new("event", vec![Some("ev-1".into()), None]));
        let window = ProbeWindow {
            before: registry.sample(&checks(&["event"])).await,
            after: registry.sample(&checks(&["event"])).await,
        };
        assert_eq!(window.produced("event"), Some(false));
    }

    #[tokio::test]
    async fn an_unregistered_check_is_unobserved_rather_than_failed() {
        // A typo in a SKILL.md must not permanently score the skill as
        // having done nothing.
        let registry = ProbeRegistry::new();
        let window = ProbeWindow {
            before: registry.sample(&checks(&["typo"])).await,
            after: registry.sample(&checks(&["typo"])).await,
        };
        assert_eq!(window.produced("typo"), None);
    }

    #[tokio::test]
    async fn a_probe_that_errors_is_unobserved_rather_than_failed() {
        // A calendar server being briefly down is not evidence about the
        // run.
        let registry = ProbeRegistry::new().with(Arc::new(Broken));
        let window = ProbeWindow {
            before: registry.sample(&checks(&["broken"])).await,
            after: registry.sample(&checks(&["broken"])).await,
        };
        assert_eq!(window.produced("broken"), None);
    }
}
