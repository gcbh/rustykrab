//! Deterministic checks against a [`Transcript`].
//!
//! Every assertion explains itself on failure. A report that says only
//! "scenario failed" costs more time than it saves, especially when the
//! thing being measured is a sampled model that failed differently on each
//! of three repetitions.

use serde_json::Value;

use crate::transcript::{ToolInvocation, Transcript};

#[derive(Debug, Clone)]
pub enum Assertion {
    /// The run completed without an infrastructure error.
    NoRunError,
    /// The agent said something.
    FinalNonEmpty,
    /// The final answer contains at least one of these (case-insensitive).
    /// Use a list when several phrasings are equally correct.
    FinalContainsAny(Vec<String>),
    /// The final answer contains all of these.
    FinalContainsAll(Vec<String>),
    /// The final answer contains none of these.
    FinalContainsNone(Vec<String>),
    /// The final answer matches this regex.
    FinalMatches(String),
    // Success means the tool ran. A call the host refused as not callable
    // in the conversation is an attempt, not the tool running, so the
    // success-style tool assertions below all read `Transcript::executed`.
    // Attempts are counted only where the attempt itself is the failure
    // (`ToolNotCalled`, `RetriesAtMost`) and reported, unjudged, through
    // `ModelCase::counting_attempts`.
    /// The named tool ran at least once.
    ToolExecuted(String),
    /// The named tool was never called. Counts attempts: for a tool the
    /// agent should not even try, trying is the failure.
    ToolNotCalled(String),
    /// The named tool never ran: every call to it, if any, was refused by
    /// the host as not callable in the conversation. Judges what executed;
    /// a refused call is the host doing its job.
    ToolNotExecuted(String),
    /// The named tool ran between `min` and `max` times inclusive.
    ToolExecutedCount {
        tool: String,
        min: usize,
        max: usize,
    },
    /// Some call to `tool` that ran had an argument at `pointer` (a JSON
    /// pointer, e.g. `/city`) whose string form contains `needle`.
    ToolExecutedArgContains {
        tool: String,
        pointer: String,
        needle: String,
    },
    /// These tools ran in this relative order (other calls may be
    /// interleaved); a refused call does not advance the order.
    ToolExecutedOrder(Vec<String>),
    /// Some result returned by `tool` contains one of these. Asserts on
    /// what the tool gave the model, separately from what the model then
    /// did with it.
    ToolOutputContainsAny { tool: String, needles: Vec<String> },
    /// The named tool ran and failed at least once and the agent still
    /// produced a final answer containing one of `then_says`: the recovery
    /// path. A refused call is not a tool failure to recover from.
    RecoveredFrom {
        tool: String,
        then_says: Vec<String>,
    },
    /// The agent called a failing tool no more than `max` times before
    /// moving on. Catches the infinite-retry failure mode, so it counts
    /// attempts: a refused call still spends a turn of the loop.
    RetriesAtMost { tool: String, max: usize },
    /// Compaction did (or didn't) run.
    Compacted(bool),

    /// The generated summary contains at least one of these — the facts
    /// that had to survive the fold.
    SummaryContainsAny(Vec<String>),
    /// At least this many characters of displaced history were archived
    /// for the recall tools. Compaction replaces the live messages, so
    /// this is where "nothing was destroyed" is actually checked.
    ArchivedAtLeast(usize),
    /// The live window shrank to at most this many messages — a
    /// compaction that does not shrink anything has not helped.
    LiveMessagesAtMost(usize),
    /// The agent finished within this many assistant turns.
    IterationsAtMost(usize),
    /// Every request of the conversation declared the same tools array, at
    /// least this many requests: the tool block was fixed from turn 0 and
    /// late tools arrived by append (plan section 12, scenario 10). Reads
    /// [`Transcript::tool_blocks`], so the caller must have read the log.
    ToolBlockUnchanged { min_requests: usize },
}

impl Assertion {
    pub fn label(&self) -> String {
        match self {
            Assertion::NoRunError => "no run error".into(),
            Assertion::FinalNonEmpty => "final answer non-empty".into(),
            Assertion::FinalContainsAny(v) => format!("final contains any {v:?}"),
            Assertion::FinalContainsAll(v) => format!("final contains all {v:?}"),
            Assertion::FinalContainsNone(v) => format!("final contains none {v:?}"),
            Assertion::FinalMatches(p) => format!("final matches /{p}/"),
            Assertion::ToolExecuted(t) => format!("ran {t}"),
            Assertion::ToolNotCalled(t) => format!("never called {t}"),
            Assertion::ToolNotExecuted(t) => format!("never ran {t}"),
            Assertion::ToolExecutedCount { tool, min, max } => {
                format!("{tool} ran {min}..={max} times")
            }
            Assertion::ToolExecutedArgContains {
                tool,
                pointer,
                needle,
            } => format!("{tool} ran with {pointer} containing {needle:?}"),
            Assertion::ToolExecutedOrder(v) => format!("run order {v:?}"),
            Assertion::ToolOutputContainsAny { tool, needles } => {
                format!("{tool} returned any {needles:?}")
            }
            Assertion::RecoveredFrom { tool, .. } => format!("recovered from {tool} failure"),
            Assertion::RetriesAtMost { tool, max } => format!("{tool} called at most {max}x"),
            Assertion::Compacted(b) => format!("compacted == {b}"),

            Assertion::SummaryContainsAny(v) => format!("summary contains any {v:?}"),
            Assertion::ArchivedAtLeast(n) => format!("archived >= {n} chars of history"),
            Assertion::LiveMessagesAtMost(n) => format!("live window <= {n} messages"),
            Assertion::IterationsAtMost(n) => format!("assistant turns <= {n}"),
            Assertion::ToolBlockUnchanged { min_requests } => {
                format!("one tool block across >= {min_requests} requests")
            }
        }
    }

    /// Evaluate against a run. `Ok(())` is a pass.
    pub fn check(&self, t: &Transcript) -> Result<(), String> {
        match self {
            Assertion::NoRunError => match &t.error {
                Some(e) => Err(format!("run failed: {e}")),
                None => Ok(()),
            },

            Assertion::FinalNonEmpty => {
                if t.final_text.trim().is_empty() {
                    Err("the agent produced no final text".into())
                } else {
                    Ok(())
                }
            }

            Assertion::FinalContainsAny(needles) => contains_any(&t.final_text, needles)
                .map_err(|m| format!("none of {m:?} in final answer: {}", excerpt(&t.final_text))),

            Assertion::FinalContainsAll(needles) => {
                contains_all(&t.final_text, needles).map_err(|m| {
                    format!(
                        "{m:?} missing from final answer: {}",
                        excerpt(&t.final_text)
                    )
                })
            }

            Assertion::FinalContainsNone(needles) => {
                let hay = t.final_text.to_lowercase();
                let found: Vec<&String> = needles
                    .iter()
                    .filter(|n| hay.contains(&n.to_lowercase()))
                    .collect();
                if found.is_empty() {
                    Ok(())
                } else {
                    Err(format!("forbidden {found:?} present in final answer"))
                }
            }

            Assertion::FinalMatches(pattern) => match regex::Regex::new(pattern) {
                Ok(re) if re.is_match(&t.final_text) => Ok(()),
                Ok(_) => Err(format!(
                    "/{pattern}/ did not match final answer: {}",
                    excerpt(&t.final_text)
                )),
                Err(e) => Err(format!("invalid regex /{pattern}/: {e}")),
            },

            Assertion::ToolExecuted(tool) => {
                let attempted = t.calls_to(tool).len();
                if !t.executed(tool).is_empty() {
                    Ok(())
                } else if attempted > 0 {
                    Err(format!(
                        "{tool} never ran: the host refused all {attempted} call(s) to it ({})",
                        called_summary(t)
                    ))
                } else {
                    Err(format!("{tool} was never called ({})", called_summary(t)))
                }
            }

            Assertion::ToolNotCalled(tool) => {
                let n = t.calls_to(tool).len();
                if n == 0 {
                    Ok(())
                } else {
                    Err(format!("{tool} was called {n}x but should not have been"))
                }
            }

            Assertion::ToolNotExecuted(tool) => {
                let n = t.executed(tool).len();
                if n == 0 {
                    Ok(())
                } else {
                    Err(format!("{tool} ran {n}x but should not have"))
                }
            }

            Assertion::ToolExecutedCount { tool, min, max } => {
                let n = t.executed(tool).len();
                if n >= *min && n <= *max {
                    Ok(())
                } else {
                    Err(format!(
                        "{tool} ran {n}x, expected {min}..={max} ({})",
                        called_summary(t)
                    ))
                }
            }

            Assertion::ToolExecutedArgContains {
                tool,
                pointer,
                needle,
            } => arg_contains(&t.executed(tool), pointer, needle).map_err(|seen| {
                format!(
                    "no {tool} call that ran had {needle:?} at {pointer}; saw {seen:?} ({})",
                    called_summary(t)
                )
            }),

            Assertion::ToolExecutedOrder(expected) => {
                let mut remaining = expected.iter();
                let mut want = remaining.next();
                for call in t.calls.iter().filter(|c| !c.refused) {
                    if Some(&call.tool) == want {
                        want = remaining.next();
                    }
                }
                match want {
                    None => Ok(()),
                    Some(missing) => Err(format!(
                        "expected run order {expected:?}; stalled at {missing} ({})",
                        called_summary(t)
                    )),
                }
            }

            Assertion::ToolOutputContainsAny { tool, needles } => {
                let outputs = t.outputs_of(tool);
                if outputs.is_empty() {
                    return Err(format!("{tool} returned nothing (was it called?)"));
                }
                contains_any(&outputs, needles)
                    .map_err(|m| format!("{tool} returned none of {m:?}: {}", excerpt(&outputs)))
            }

            Assertion::RecoveredFrom { tool, then_says } => {
                if !t.executed(tool).iter().any(|c| c.failed) {
                    return Err(format!(
                        "{tool} never ran and failed, so there was nothing to recover from ({})",
                        called_summary(t)
                    ));
                }
                contains_any(&t.final_text, then_says).map_err(|m| {
                    format!(
                        "{tool} failed but the agent never reported any of {m:?}: {}",
                        excerpt(&t.final_text)
                    )
                })
            }

            Assertion::RetriesAtMost { tool, max } => {
                let n = t.calls_to(tool).len();
                if n <= *max {
                    Ok(())
                } else {
                    Err(format!("{tool} called {n}x, more than the {max} allowed"))
                }
            }

            Assertion::Compacted(want) => {
                if t.compacted == *want {
                    Ok(())
                } else if *want {
                    Err(format!(
                        "no compaction ran ({} messages in the window)",
                        t.live_messages
                    ))
                } else {
                    Err("compaction ran but should not have".into())
                }
            }

            Assertion::SummaryContainsAny(needles) => match &t.summary {
                None => Err("no summary was produced".into()),
                Some(s) => contains_any(s, needles)
                    .map_err(|m| format!("none of {m:?} in summary: {}", excerpt(s))),
            },

            Assertion::ArchivedAtLeast(n) => {
                if t.archived_chars >= *n {
                    Ok(())
                } else {
                    Err(format!(
                        "{} chars archived, expected >= {n} — compaction dropped the \
                         displaced history instead of preserving it for recall",
                        t.archived_chars
                    ))
                }
            }

            Assertion::LiveMessagesAtMost(n) => {
                if t.live_messages <= *n {
                    Ok(())
                } else {
                    Err(format!(
                        "{} messages still live, expected <= {n} — the compaction did not \
                         shrink the window",
                        t.live_messages
                    ))
                }
            }

            Assertion::IterationsAtMost(n) => {
                let turns = t.assistant_texts.len();
                if turns <= *n {
                    Ok(())
                } else {
                    Err(format!("{turns} assistant turns, expected <= {n}"))
                }
            }

            Assertion::ToolBlockUnchanged { min_requests } => {
                match crate::tool_blocks::unchanged(&t.tool_blocks, *min_requests) {
                    None => Ok(()),
                    Some(why) => Err(why),
                }
            }
        }
    }
}

/// Case-insensitive "contains at least one". On failure returns the whole
/// candidate list, since none of them matched.
fn contains_any(haystack: &str, needles: &[String]) -> Result<(), Vec<String>> {
    let hay = haystack.to_lowercase();
    if needles.iter().any(|n| hay.contains(&n.to_lowercase())) {
        Ok(())
    } else {
        Err(needles.to_vec())
    }
}

/// Case-insensitive "contains every one". On failure returns only what is
/// missing, which is what you need to see.
fn contains_all(haystack: &str, needles: &[String]) -> Result<(), Vec<String>> {
    let hay = haystack.to_lowercase();
    let missing: Vec<String> = needles
        .iter()
        .filter(|n| !hay.contains(&n.to_lowercase()))
        .cloned()
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(missing)
    }
}

/// Case-insensitive "some call has `needle` at `pointer`". On failure
/// returns the values seen there.
fn arg_contains(calls: &[&ToolInvocation], pointer: &str, needle: &str) -> Result<(), Vec<String>> {
    let want = needle.to_lowercase();
    let seen: Vec<String> = calls
        .iter()
        .filter_map(|c| c.args.pointer(pointer).map(render))
        .collect();
    if seen.iter().any(|v| v.to_lowercase().contains(&want)) {
        Ok(())
    } else {
        Err(seen)
    }
}

/// JSON values stringify with quotes; bare strings should not.
fn render(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn excerpt(s: &str) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= 200 {
        format!("{flat:?}")
    } else {
        let head: String = flat.chars().take(200).collect();
        format!("{head:?}…")
    }
}

fn called_summary(t: &Transcript) -> String {
    if t.calls.is_empty() {
        return "no tools were called".to_string();
    }
    format!(
        "called: {}",
        t.calls
            .iter()
            .map(|c| {
                let mark = if c.refused {
                    ":refused"
                } else if c.failed {
                    ":error"
                } else {
                    ""
                };
                format!("{}{mark}", c.tool)
            })
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Terse constructor — the suites read better without `.to_string()` noise.
pub fn s(items: &[&str]) -> Vec<String> {
    items.iter().map(|i| i.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn with_calls(calls: Vec<ToolInvocation>) -> Transcript {
        Transcript {
            calls,
            ..Default::default()
        }
    }

    fn call(tool: &str, args: Value, failed: bool) -> ToolInvocation {
        ToolInvocation {
            tool: tool.to_string(),
            args,
            output: None,
            failed,
            refused: false,
        }
    }

    #[test]
    fn not_executed_ignores_calls_the_host_refused() {
        let mut refused = call("get_forecast", json!({}), true);
        refused.refused = true;
        let t = with_calls(vec![refused.clone()]);
        assert!(Assertion::ToolNotExecuted("get_forecast".into())
            .check(&t)
            .is_ok());
        assert!(Assertion::ToolNotCalled("get_forecast".into())
            .check(&t)
            .is_err());
        let t = with_calls(vec![refused, call("get_forecast", json!({}), false)]);
        assert!(Assertion::ToolNotExecuted("get_forecast".into())
            .check(&t)
            .is_err());
    }

    #[test]
    fn executed_requires_a_call_that_ran() {
        let mut refused = call("get_weather", json!({ "city": "Lisbon" }), true);
        refused.refused = true;
        let ran = Assertion::ToolExecuted("get_weather".into());
        let ran_with = Assertion::ToolExecutedArgContains {
            tool: "get_weather".into(),
            pointer: "/city".into(),
            needle: "lisbon".into(),
        };

        // A refused call is an attempt, which the executed assertions do
        // not count.
        let t = with_calls(vec![refused.clone()]);
        let why = ran.check(&t).unwrap_err();
        assert!(why.contains("refused"), "{why}");
        assert!(ran_with.check(&t).is_err());

        assert!(ran.check(&with_calls(vec![])).is_err());

        // The key argument must be on a call that ran.
        let t = with_calls(vec![
            refused.clone(),
            call("get_weather", json!({ "city": "Porto" }), false),
        ]);
        assert!(ran.check(&t).is_ok());
        assert!(ran_with.check(&t).is_err());

        let t = with_calls(vec![
            refused,
            call("get_weather", json!({ "city": "Lisbon" }), false),
        ]);
        assert!(ran.check(&t).is_ok());
        assert!(ran_with.check(&t).is_ok());
    }

    fn refused(tool: &str) -> ToolInvocation {
        let mut c = call(tool, json!({}), true);
        c.refused = true;
        c
    }

    #[test]
    fn executed_count_ignores_calls_the_host_refused() {
        let count = |min, max| Assertion::ToolExecutedCount {
            tool: "weather".into(),
            min,
            max,
        };
        let t = with_calls(vec![
            refused("weather"),
            refused("weather"),
            call("weather", json!({}), false),
        ]);
        assert!(count(1, 1).check(&t).is_ok());
        let why = count(2, 3).check(&t).unwrap_err();
        assert!(why.contains("ran 1x"), "{why}");
        assert!(count(1, 3)
            .check(&with_calls(vec![refused("weather")]))
            .is_err());
    }

    #[test]
    fn executed_order_skips_calls_the_host_refused() {
        let order = Assertion::ToolExecutedOrder(s(&["search", "fetch"]));
        // Only a refused call puts `search` first: attempted in order,
        // never run in order.
        let t = with_calls(vec![
            refused("search"),
            call("fetch", json!({}), false),
            call("search", json!({}), false),
        ]);
        let why = order.check(&t).unwrap_err();
        assert!(why.contains("stalled at fetch"), "{why}");

        let t = with_calls(vec![
            call("search", json!({}), false),
            refused("noise"),
            call("fetch", json!({}), false),
        ]);
        assert!(order.check(&t).is_ok());
    }

    #[test]
    fn recovery_needs_a_failure_that_ran() {
        let recovered = Assertion::RecoveredFrom {
            tool: "flaky".into(),
            then_says: s(&["all good"]),
        };
        let mut t = with_calls(vec![refused("flaky"), call("flaky", json!({}), false)]);
        t.final_text = "all good".into();
        assert!(
            recovered.check(&t).is_err(),
            "a refusal is the host declining, not the tool failing"
        );
        t.calls[1].failed = true;
        assert!(recovered.check(&t).is_ok());
    }

    #[test]
    fn contains_any_is_case_insensitive() {
        let t = Transcript {
            final_text: "The Weather in Reykjavik is COLD".into(),
            ..Default::default()
        };
        assert!(Assertion::FinalContainsAny(s(&["reykjavik"]))
            .check(&t)
            .is_ok());
        assert!(Assertion::FinalContainsAny(s(&["oslo"])).check(&t).is_err());
    }

    #[test]
    fn call_order_allows_interleaving_but_not_reordering() {
        let t = with_calls(vec![
            call("search", json!({}), false),
            call("noise", json!({}), false),
            call("fetch", json!({}), false),
        ]);
        assert!(Assertion::ToolExecutedOrder(s(&["search", "fetch"]))
            .check(&t)
            .is_ok());
        assert!(Assertion::ToolExecutedOrder(s(&["fetch", "search"]))
            .check(&t)
            .is_err());
    }

    #[test]
    fn tool_arg_contains_reads_json_pointers() {
        let t = with_calls(vec![call(
            "weather",
            json!({ "location": { "city": "Reykjavik" } }),
            false,
        )]);
        assert!(Assertion::ToolExecutedArgContains {
            tool: "weather".into(),
            pointer: "/location/city".into(),
            needle: "reykjavik".into(),
        }
        .check(&t)
        .is_ok());
    }

    #[test]
    fn recovery_requires_an_actual_failure() {
        let mut t = with_calls(vec![call("flaky", json!({}), false)]);
        t.final_text = "all good".into();
        let assertion = Assertion::RecoveredFrom {
            tool: "flaky".into(),
            then_says: s(&["all good"]),
        };
        assert!(
            assertion.check(&t).is_err(),
            "a scenario that never broke anything must not claim recovery"
        );

        t.calls[0].failed = true;
        assert!(assertion.check(&t).is_ok());
    }

    #[test]
    fn tool_output_assertions_see_what_the_tool_returned() {
        let mut t = with_calls(vec![call("memory_search", json!({}), false)]);
        t.calls[0].output = Some(json!({ "memories": [{ "content": "kettle is a Stagg EKG" }] }));
        assert!(Assertion::ToolOutputContainsAny {
            tool: "memory_search".into(),
            needles: s(&["stagg"]),
        }
        .check(&t)
        .is_ok());
    }
}
