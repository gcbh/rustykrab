//! Classifier rules an `internal` item lands at run time (plan section 9,
//! scenario 14).
//!
//! An `unknown` failure files an `internal` item that asks for "the probe,
//! log line, check or parser that would have classified" it. When that
//! work is a rule the message table did not have, the item's worker reports
//! it as a `classifier_rule` artifact, `<subclass>: <pattern>`, and the
//! controller checks it the way the item's `done_when` says: replaying the
//! failure the item was filed for through the classifier with the rule
//! must yield a class other than `unknown`. A rule that passes is kept as
//! the item's evidence, so it outlives restarts and aging, and every later
//! failure the built-in table leaves `unknown` is matched against the kept
//! rules. The built-in table ([`super::MESSAGE_RULES`]) still decides
//! first: a learned rule only ever classifies what nothing else did.

use rustykrab_core::work::{ErrorClass, ErrorSubclass, WorkError, WorkItemId};

use super::{fingerprint, Context};

/// The evidence and artifact kind a landed rule travels as.
pub const CLASSIFIER_RULE: &str = "classifier_rule";

/// One learned row: the subclass a message means when it contains
/// `pattern` (lowercased, as the message table's text patterns match).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LearnedRule {
    /// The `internal` item that landed it.
    pub item: WorkItemId,
    pub subclass: ErrorSubclass,
    pub pattern: String,
}

impl LearnedRule {
    /// Parse `<subclass>: <pattern>`. The subclass is one of the taxonomy's
    /// names and may not be `unclassified`; the pattern must be at least
    /// three characters, so a rule cannot swallow every message.
    pub fn parse(item: &str, raw: &str) -> Result<LearnedRule, String> {
        let (name, pattern) = raw
            .split_once(':')
            .ok_or_else(|| format!("`{raw}` is not `<subclass>: <pattern>`"))?;
        let name = name.trim();
        let subclass = ErrorSubclass::parse(name)
            .ok_or_else(|| format!("`{name}` is not an error subclass"))?;
        if subclass == ErrorSubclass::Unclassified {
            return Err("a rule must classify: `unclassified` is what it replaces".into());
        }
        let pattern = pattern.trim().to_lowercase();
        if pattern.chars().count() < 3 {
            return Err(format!(
                "the pattern `{pattern}` is too short to be specific"
            ));
        }
        Ok(LearnedRule {
            item: item.to_string(),
            subclass,
            pattern,
        })
    }

    /// Whether the rule matches `message`.
    pub fn matches(&self, message: &str) -> bool {
        message.to_lowercase().contains(&self.pattern)
    }

    /// `observed_by` of an error this rule classified.
    pub fn observed_by(&self) -> String {
        let short: String = self.item.chars().take(8).collect();
        format!("rule:learned:{short}")
    }
}

/// `error` classified by the first learned rule its detail matches, when
/// the built-in classifiers left it `unknown`; otherwise `error` unchanged.
pub fn apply_learned(error: WorkError, rules: &[LearnedRule], ctx: &Context) -> WorkError {
    if error.class != ErrorClass::Unknown {
        return error;
    }
    let Some(rule) = rules.iter().find(|r| r.matches(&error.detail)) else {
        return error;
    };
    let class = rule.subclass.class();
    WorkError {
        class,
        subclass: rule.subclass,
        fingerprint: fingerprint(class, rule.subclass, None, ctx.worker_kind, &error.detail),
        detail: error.detail,
        artifact_refs: error.artifact_refs,
        observed_by: rule.observed_by(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::{classify, FailureInput};

    fn ctx() -> Context<'static> {
        Context {
            tool: None,
            worker_kind: None,
        }
    }

    fn unknown(detail: &str) -> WorkError {
        classify(
            &FailureInput::Raw {
                message: detail.to_string(),
            },
            &ctx(),
        )
    }

    #[test]
    fn a_rule_parses_from_subclass_and_pattern() {
        let rule = LearnedRule::parse("item-1", "process: E2E-ZQX-17").unwrap();
        assert_eq!(rule.subclass, ErrorSubclass::Process);
        assert_eq!(rule.pattern, "e2e-zqx-17");
        assert_eq!(rule.observed_by(), "rule:learned:item-1");
        for bad in ["no colon", "bogus: zqx", "unclassified: zqx", "process: ab"] {
            assert!(LearnedRule::parse("i", bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn scenario_14_a_landed_rule_classifies_the_failure_on_replay() {
        let failure = unknown("E2E-ZQX-17 flux capacitor desynchronised");
        assert_eq!(failure.class, ErrorClass::Unknown);
        let rules = vec![LearnedRule::parse("internal-1", "process: e2e-zqx-17").unwrap()];
        let again = apply_learned(failure.clone(), &rules, &ctx());
        assert_eq!(again.class, ErrorClass::Environment);
        assert_eq!(again.subclass, ErrorSubclass::Process);
        assert_eq!(again.observed_by, "rule:learned:internal");
        assert_eq!(again.detail, failure.detail);
        assert_ne!(again.fingerprint, failure.fingerprint);
    }

    #[test]
    fn a_learned_rule_never_overrides_the_built_in_table() {
        let known = classify(
            &FailureInput::Raw {
                message: "connection refused while fetching".into(),
            },
            &ctx(),
        );
        assert_ne!(known.class, ErrorClass::Unknown);
        let rules = vec![LearnedRule::parse("i", "disk: connection refused").unwrap()];
        assert_eq!(apply_learned(known.clone(), &rules, &ctx()), known);
        let other = unknown("something else entirely went sideways");
        assert_eq!(apply_learned(other.clone(), &rules, &ctx()), other);
    }
}
