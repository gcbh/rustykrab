//! Deterministic checks against a [`Transcript`].
//!
//! Every assertion explains itself on failure. A report that says only
//! "scenario failed" costs more time than it saves, especially when the
//! thing being measured is a sampled model that failed differently on each
//! of three repetitions.

use serde_json::Value;

use crate::transcript::Transcript;

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
    /// The named tool was called at least once.
    ToolCalled(String),
    /// The named tool was never called.
    ToolNotCalled(String),
    /// The named tool was called between `min` and `max` times inclusive.
    ToolCallCount {
        tool: String,
        min: usize,
        max: usize,
    },
    /// Some call to `tool` had an argument at `pointer` (a JSON pointer,
    /// e.g. `/city`) whose string form contains `needle`.
    ToolArgContains {
        tool: String,
        pointer: String,
        needle: String,
    },
    /// These tools were called in this relative order (other calls may be
    /// interleaved).
    ToolCallOrder(Vec<String>),
    /// Some result returned by `tool` contains one of these. Asserts on
    /// what the tool gave the model, separately from what the model then
    /// did with it.
    ToolOutputContainsAny { tool: String, needles: Vec<String> },
    /// The named tool failed at least once and the agent still produced a
    /// final answer containing one of `then_says` — the recovery path.
    RecoveredFrom {
        tool: String,
        then_says: Vec<String>,
    },
    /// The agent called a failing tool no more than `max` times before
    /// moving on. Catches the infinite-retry failure mode.
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
            Assertion::ToolCalled(t) => format!("called {t}"),
            Assertion::ToolNotCalled(t) => format!("never called {t}"),
            Assertion::ToolCallCount { tool, min, max } => {
                format!("{tool} called {min}..={max} times")
            }
            Assertion::ToolArgContains {
                tool,
                pointer,
                needle,
            } => format!("{tool}{pointer} contains {needle:?}"),
            Assertion::ToolCallOrder(v) => format!("call order {v:?}"),
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

            Assertion::ToolCalled(tool) => {
                if t.calls_to(tool).is_empty() {
                    Err(format!("{tool} was never called ({})", called_summary(t)))
                } else {
                    Ok(())
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

            Assertion::ToolCallCount { tool, min, max } => {
                let n = t.calls_to(tool).len();
                if n >= *min && n <= *max {
                    Ok(())
                } else {
                    Err(format!("{tool} called {n}x, expected {min}..={max}"))
                }
            }

            Assertion::ToolArgContains {
                tool,
                pointer,
                needle,
            } => {
                let want = needle.to_lowercase();
                let seen: Vec<String> = t
                    .calls_to(tool)
                    .iter()
                    .filter_map(|c| c.args.pointer(pointer).map(render))
                    .collect();
                if seen.iter().any(|v| v.to_lowercase().contains(&want)) {
                    Ok(())
                } else {
                    Err(format!(
                        "no {tool} call had {needle:?} at {pointer}; saw {seen:?}"
                    ))
                }
            }

            Assertion::ToolCallOrder(expected) => {
                let mut remaining = expected.iter();
                let mut want = remaining.next();
                for call in &t.calls {
                    if Some(&call.tool) == want {
                        want = remaining.next();
                    }
                }
                match want {
                    None => Ok(()),
                    Some(missing) => Err(format!(
                        "expected order {expected:?}; stalled at {missing} ({})",
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
                let calls = t.calls_to(tool);
                if !calls.iter().any(|c| c.failed) {
                    return Err(format!(
                        "{tool} never failed, so there was nothing to recover from"
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
            .map(|c| format!("{}{}", c.tool, if c.failed { ":error" } else { "" }))
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
    use crate::transcript::ToolInvocation;
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
        }
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
        assert!(Assertion::ToolCallOrder(s(&["search", "fetch"]))
            .check(&t)
            .is_ok());
        assert!(Assertion::ToolCallOrder(s(&["fetch", "search"]))
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
        assert!(Assertion::ToolArgContains {
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
