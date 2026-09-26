//! Deterministic classification: [`FailureInput`] to [`WorkError`].
//!
//! The shape of the input decides first. A provider problem, a verifier
//! verdict, a policy stop, a spent budget and a capability check each name
//! their subclass outright. A tool result's [`ToolErrorKind`] names it for a
//! timeout or a rate limit, and otherwise sets which subclasses the message
//! rules may refine it to, so an `invalid_input` whose message mentions a
//! timeout is still `invalid_args`. A process exit is read from its code,
//! then its stderr. A raw message, and a tool result of kind `internal`, go
//! through every rule; what none matches is `unknown/unclassified`.

use std::borrow::Cow;

use rustykrab_core::work::{ErrorSubclass, WorkError};
use rustykrab_core::ToolErrorKind;

use super::rules::{first_match, MessageRule, MESSAGE_RULES};
use super::{
    fingerprint, gap_detail, Context, FailureInput, GapKind, ProviderProblem, UNCLASSIFIED,
};

/// Details longer than this are clipped; the evidence refs carry the rest.
const DETAIL_MAX_CHARS: usize = 500;

/// Classify with the built-in rule table.
pub fn classify(input: &FailureInput, ctx: &Context) -> WorkError {
    classify_with(MESSAGE_RULES, input, ctx)
}

/// Classify with a given rule table, so a new rule can be replayed against
/// recorded evidence before it lands in [`MESSAGE_RULES`].
pub fn classify_with(rules: &[MessageRule], input: &FailureInput, ctx: &Context) -> WorkError {
    let found = observe(rules, input, ctx);
    let subclass = found.subclass;
    let class = subclass.class();
    let observed_by = if subclass == ErrorSubclass::Unclassified {
        UNCLASSIFIED.to_string()
    } else {
        found.observed_by
    };
    let detail = match found.gap {
        Some(gap) => gap_detail(gap, &found.detail),
        None => found.detail,
    };
    WorkError {
        class,
        subclass,
        fingerprint: fingerprint(class, subclass, found.tool, ctx.worker_kind, &found.message),
        detail: clip(detail),
        artifact_refs: Vec::new(),
        observed_by,
    }
}

/// What one input says, before it becomes a [`WorkError`].
struct Found<'a> {
    subclass: ErrorSubclass,
    /// Set when the result is a capability gap; the detail is then written
    /// as `needs <gap>: <detail>`.
    gap: Option<GapKind>,
    observed_by: String,
    detail: String,
    /// What the fingerprint hashes, after normalising.
    message: Cow<'a, str>,
    tool: Option<&'a str>,
}

fn observe<'a>(rules: &[MessageRule], input: &'a FailureInput, ctx: &Context<'a>) -> Found<'a> {
    match input {
        FailureInput::ToolResult {
            tool,
            kind,
            message,
        } => {
            let tool = Some(tool.as_str()).filter(|t| !t.is_empty()).or(ctx.tool);
            let detail = match tool {
                Some(t) => labelled(t, message),
                None => message.clone(),
            };
            let (subclass, gap, observed_by) = tool_result(rules, *kind, message);
            Found {
                subclass,
                gap,
                observed_by,
                detail,
                message: Cow::Borrowed(message),
                tool,
            }
        }
        FailureInput::Provider { problem, detail } => {
            let label = match problem {
                ProviderProblem::Unavailable => "unavailable",
                other => other.subclass().as_str(),
            };
            Found {
                subclass: problem.subclass(),
                gap: None,
                observed_by: "provider".into(),
                detail: labelled(&format!("provider {label}"), detail),
                message: Cow::Borrowed(detail),
                tool: ctx.tool,
            }
        }
        FailureInput::ProcessExit { code, stderr_tail } => {
            let detail = match code {
                Some(c) => labelled(&format!("exit {c}"), stderr_tail),
                None => labelled("killed by signal", stderr_tail),
            };
            let (subclass, gap, observed_by) = process_exit(rules, *code, stderr_tail);
            Found {
                subclass,
                gap,
                observed_by,
                detail,
                message: Cow::Borrowed(stderr_tail),
                tool: ctx.tool,
            }
        }
        FailureInput::Verifier { verdict, detail } => Found {
            subclass: verdict.subclass(),
            gap: None,
            observed_by: "verifier".into(),
            detail: labelled(verdict.subclass().as_str(), detail),
            message: Cow::Borrowed(detail),
            tool: ctx.tool,
        },
        FailureInput::Policy { stop, detail } => Found {
            subclass: stop.subclass(),
            gap: None,
            observed_by: "policy".into(),
            detail: labelled(&format!("policy {}", stop.subclass().as_str()), detail),
            message: Cow::Borrowed(detail),
            tool: ctx.tool,
        },
        FailureInput::BudgetExhausted { budget, detail } => Found {
            subclass: budget.subclass(),
            gap: None,
            observed_by: "budget".into(),
            detail: labelled(
                &format!("{} budget spent", budget.subclass().as_str()),
                detail,
            ),
            message: Cow::Borrowed(detail),
            tool: ctx.tool,
        },
        FailureInput::CapabilityGap { gap, name } => Found {
            subclass: gap.subclass(),
            gap: Some(*gap),
            observed_by: "capability_check".into(),
            detail: name.clone(),
            // The gap kind is part of what is hashed, so a capacity gap and a
            // compute gap on the same name stay apart.
            message: Cow::Owned(gap_detail(*gap, name)),
            tool: ctx.tool,
        },
        FailureInput::Raw { message } => {
            let (subclass, gap, observed_by) = by_rule(rules, message, |_| true).unwrap_or((
                ErrorSubclass::Unclassified,
                None,
                UNCLASSIFIED.into(),
            ));
            Found {
                subclass,
                gap,
                observed_by,
                detail: message.clone(),
                message: Cow::Borrowed(message),
                tool: ctx.tool,
            }
        }
    }
}

type Verdict = (ErrorSubclass, Option<GapKind>, String);

fn by_rule(
    rules: &[MessageRule],
    message: &str,
    allowed: impl Fn(ErrorSubclass) -> bool,
) -> Option<Verdict> {
    first_match(rules, message, allowed).map(|r| (r.subclass, r.gap, format!("rule:{}", r.name)))
}

/// A tool result: the kind decides, or bounds what the rules may say.
fn tool_result(rules: &[MessageRule], kind: ToolErrorKind, message: &str) -> Verdict {
    use ErrorSubclass as S;
    let by_kind = |subclass: ErrorSubclass| -> Verdict {
        (subclass, None, format!("tool_result:{}", kind.as_str()))
    };
    match kind {
        ToolErrorKind::Timeout => by_kind(S::Timeout),
        ToolErrorKind::RateLimited => by_kind(S::UpstreamError),
        ToolErrorKind::InvalidInput => by_rule(rules, message, |s| {
            matches!(s, S::HallucinatedTool | S::Compute)
        })
        .unwrap_or_else(|| by_kind(S::InvalidArgs)),
        ToolErrorKind::NotFound => by_rule(rules, message, |s| {
            matches!(s, S::HallucinatedTool | S::Dependency | S::ToolGap)
        })
        .unwrap_or_else(|| by_kind(S::NotFound)),
        ToolErrorKind::PermissionDenied => by_rule(rules, message, |s| {
            matches!(s, S::Credential | S::Consent | S::Disk)
        })
        .unwrap_or_else(|| by_kind(S::Permission)),
        ToolErrorKind::Transient => by_rule(rules, message, |s| {
            matches!(s, S::Network | S::Disk | S::Timeout | S::UpstreamError)
        })
        .unwrap_or_else(|| by_kind(S::UpstreamError)),
        // The catch-all for untyped errors: only the message can say.
        ToolErrorKind::Internal => by_rule(rules, message, |_| true).unwrap_or((
            S::Unclassified,
            None,
            UNCLASSIFIED.into(),
        )),
    }
}

/// A process exit: the shell's conventional codes first, then stderr.
fn process_exit(rules: &[MessageRule], code: Option<i32>, stderr: &str) -> Verdict {
    let by_code = |subclass: ErrorSubclass, gap: Option<GapKind>, c: i32| -> Verdict {
        (subclass, gap, format!("exit_code:{c}"))
    };
    match code {
        Some(127) => by_code(ErrorSubclass::Dependency, Some(GapKind::Install), 127),
        Some(126) => by_code(ErrorSubclass::Permission, None, 126),
        Some(124) => by_code(ErrorSubclass::Timeout, None, 124),
        _ => by_rule(rules, stderr, |_| true).unwrap_or_else(|| match code {
            // A clean exit reported as a failure says nothing code can name.
            Some(0) => (ErrorSubclass::Unclassified, None, UNCLASSIFIED.into()),
            _ => (ErrorSubclass::Process, None, "exit_code".into()),
        }),
    }
}

fn labelled(label: &str, text: &str) -> String {
    let text = text.trim();
    if text.is_empty() {
        label.to_string()
    } else {
        format!("{label}: {text}")
    }
}

fn clip(mut s: String) -> String {
    if let Some((at, _)) = s.char_indices().nth(DETAIL_MAX_CHARS) {
        s.truncate(at);
        s.push_str("...");
    }
    s
}

#[cfg(test)]
mod tests {
    use rustykrab_core::work::{ErrorClass, WorkerKind};

    use super::super::{gap_of, BudgetKind, PolicyStop, VerifierVerdict};
    use super::*;

    fn ctx() -> Context<'static> {
        Context::default()
    }

    fn tool(kind: ToolErrorKind, message: &str) -> FailureInput {
        FailureInput::ToolResult {
            tool: "web_fetch".into(),
            kind,
            message: message.into(),
        }
    }

    fn raw(message: &str) -> FailureInput {
        FailureInput::Raw {
            message: message.into(),
        }
    }

    fn sub(input: &FailureInput) -> ErrorSubclass {
        classify(input, &ctx()).subclass
    }

    /// Every subclass, with a match that stops compiling when one is added.
    fn all_subclasses() -> Vec<ErrorSubclass> {
        use ErrorSubclass::*;
        let all = vec![
            InvalidArgs,
            NotFound,
            Timeout,
            UpstreamError,
            Format,
            Refusal,
            HallucinatedTool,
            Loop,
            Empty,
            ToolGap,
            Credential,
            Consent,
            Compute,
            Knowledge,
            Network,
            Disk,
            Permission,
            Process,
            Dependency,
            ClaimMismatch,
            CheckFailed,
            Incomplete,
            Scope,
            SingleWriter,
            Ceiling,
            Iterations,
            Tokens,
            Wall,
            Repairs,
            Unclassified,
        ];
        for s in &all {
            match s {
                InvalidArgs | NotFound | Timeout | UpstreamError | Format | Refusal
                | HallucinatedTool | Loop | Empty | ToolGap | Credential | Consent | Compute
                | Knowledge | Network | Disk | Permission | Process | Dependency
                | ClaimMismatch | CheckFailed | Incomplete | Scope | SingleWriter | Ceiling
                | Iterations | Tokens | Wall | Repairs | Unclassified => {}
            }
        }
        all
    }

    #[test]
    fn tool_error_kinds_map_deterministically() {
        use ErrorSubclass as S;
        let cases = [
            (
                ToolErrorKind::InvalidInput,
                S::InvalidArgs,
                "tool_result:invalid_input",
            ),
            (
                ToolErrorKind::NotFound,
                S::NotFound,
                "tool_result:not_found",
            ),
            (
                ToolErrorKind::PermissionDenied,
                S::Permission,
                "tool_result:permission_denied",
            ),
            (ToolErrorKind::Timeout, S::Timeout, "tool_result:timeout"),
            (
                ToolErrorKind::RateLimited,
                S::UpstreamError,
                "tool_result:rate_limited",
            ),
            (
                ToolErrorKind::Transient,
                S::UpstreamError,
                "tool_result:transient",
            ),
            (ToolErrorKind::Internal, S::Unclassified, UNCLASSIFIED),
        ];
        for (kind, want, observed_by) in cases {
            let e = classify(&tool(kind, "it went wrong"), &ctx());
            assert_eq!(e.subclass, want, "{kind:?}");
            assert_eq!(e.class, want.class(), "{kind:?}");
            assert_eq!(e.observed_by, observed_by, "{kind:?}");
            assert_eq!(e.detail, "web_fetch: it went wrong");
        }
    }

    #[test]
    fn messages_refine_untyped_and_raw_errors() {
        use ErrorSubclass as S;
        let cases = [
            ("request timed out after 30s", S::Timeout),
            (
                "dial tcp 10.0.0.1:443: connect: connection refused",
                S::Network,
            ),
            ("HTTP 429 Too Many Requests", S::UpstreamError),
            ("ENOENT: no such file or directory", S::NotFound),
            ("open /root/x: permission denied", S::Permission),
            ("invalid JSON at line 1 column 7", S::InvalidArgs),
            ("No space left on device", S::Disk),
        ];
        for (message, want) in cases {
            for input in [tool(ToolErrorKind::Internal, message), raw(message)] {
                let e = classify(&input, &ctx());
                assert_eq!(e.subclass, want, "{message}");
                assert!(e.observed_by.starts_with("rule:"), "{}", e.observed_by);
            }
        }
    }

    #[test]
    fn a_typed_kind_is_refined_only_within_its_bounds() {
        use ErrorSubclass as S;
        // A message that mentions a timeout does not make bad input transient.
        assert_eq!(
            sub(&tool(
                ToolErrorKind::InvalidInput,
                "timeout must be positive"
            )),
            S::InvalidArgs
        );
        assert_eq!(
            sub(&tool(ToolErrorKind::PermissionDenied, "invalid API key")),
            S::Credential
        );
        assert_eq!(
            sub(&tool(ToolErrorKind::NotFound, "sh: rg: command not found")),
            S::Dependency
        );
        assert_eq!(
            sub(&tool(ToolErrorKind::Transient, "connection reset by peer")),
            S::Network
        );
        assert_eq!(
            sub(&tool(ToolErrorKind::Transient, "no space left on device")),
            S::Disk
        );
        // Transient never becomes a non-transient programming error.
        assert_eq!(
            sub(&tool(ToolErrorKind::Transient, "invalid json in reply")),
            S::UpstreamError
        );
    }

    #[test]
    fn a_gap_found_by_a_rule_names_its_subject() {
        let e = classify(
            &tool(ToolErrorKind::PermissionDenied, "invalid API key"),
            &ctx(),
        );
        assert_eq!(e.detail, "needs credential: web_fetch: invalid API key");
        let gap = gap_of(&e).unwrap();
        assert_eq!(gap.kind, GapKind::Credential);
        assert_eq!(gap.subject, "web_fetch: invalid API key");
    }

    #[test]
    fn anything_unrecognised_is_unknown_and_unclassified() {
        for input in [
            raw("flux capacitor desynchronised"),
            raw(""),
            tool(ToolErrorKind::Internal, "the frobnicator sulked"),
            FailureInput::ProcessExit {
                code: Some(0),
                stderr_tail: "all good".into(),
            },
        ] {
            let e = classify(&input, &ctx());
            assert_eq!(e.class, ErrorClass::Unknown, "{input:?}");
            assert_eq!(e.subclass, ErrorSubclass::Unclassified, "{input:?}");
            assert_eq!(e.observed_by, UNCLASSIFIED, "{input:?}");
        }
    }

    #[test]
    fn process_exits_read_the_code_then_stderr() {
        use ErrorSubclass as S;
        let exit = |code, stderr: &str| FailureInput::ProcessExit {
            code,
            stderr_tail: stderr.into(),
        };
        assert_eq!(sub(&exit(Some(127), "jq: not here")), S::Dependency);
        assert_eq!(sub(&exit(Some(126), "")), S::Permission);
        assert_eq!(sub(&exit(Some(124), "")), S::Timeout);
        assert_eq!(
            sub(&exit(Some(1), "curl: (7) Connection refused")),
            S::Network
        );
        assert_eq!(sub(&exit(Some(2), "error: 3 tests failed")), S::Process);
        assert_eq!(sub(&exit(None, "")), S::Process);
        let e = classify(&exit(Some(2), "  error: 3 tests failed\n"), &ctx());
        assert_eq!(e.detail, "exit 2: error: 3 tests failed");
        assert_eq!(e.observed_by, "exit_code");
        assert_eq!(
            classify(&exit(Some(127), "rg"), &ctx()).detail,
            "needs install: exit 127: rg"
        );
    }

    #[test]
    fn every_subclass_is_reachable_from_some_input() {
        let inputs = vec![
            tool(ToolErrorKind::InvalidInput, "bad"),
            tool(ToolErrorKind::NotFound, "gone"),
            tool(ToolErrorKind::Timeout, "slow"),
            tool(ToolErrorKind::RateLimited, "slow down"),
            FailureInput::Provider {
                problem: ProviderProblem::Format,
                detail: "unparseable tool call".into(),
            },
            FailureInput::Provider {
                problem: ProviderProblem::Refusal,
                detail: "declined".into(),
            },
            FailureInput::Provider {
                problem: ProviderProblem::HallucinatedTool,
                detail: "web_serch".into(),
            },
            FailureInput::Provider {
                problem: ProviderProblem::Loop,
                detail: "same call three times".into(),
            },
            FailureInput::Provider {
                problem: ProviderProblem::Empty,
                detail: String::new(),
            },
        ]
        .into_iter()
        .chain(GapKind::ALL.iter().map(|g| FailureInput::CapabilityGap {
            gap: *g,
            name: "thing".into(),
        }))
        .chain([
            raw("connection refused"),
            raw("no space left on device"),
            tool(ToolErrorKind::PermissionDenied, "nope"),
            FailureInput::ProcessExit {
                code: Some(1),
                stderr_tail: "boom".into(),
            },
            FailureInput::ProcessExit {
                code: Some(127),
                stderr_tail: String::new(),
            },
        ])
        .chain(
            [
                VerifierVerdict::ClaimMismatch,
                VerifierVerdict::CheckFailed,
                VerifierVerdict::Incomplete,
            ]
            .map(|verdict| FailureInput::Verifier {
                verdict,
                detail: "d".into(),
            }),
        )
        .chain(
            [
                PolicyStop::Scope,
                PolicyStop::SingleWriter,
                PolicyStop::Ceiling,
            ]
            .map(|stop| FailureInput::Policy {
                stop,
                detail: "d".into(),
            }),
        )
        .chain(
            [
                BudgetKind::Iterations,
                BudgetKind::Tokens,
                BudgetKind::Wall,
                BudgetKind::Repairs,
            ]
            .map(|budget| FailureInput::BudgetExhausted {
                budget,
                detail: "d".into(),
            }),
        )
        .chain([raw("???")]);

        let mut seen: Vec<ErrorSubclass> = Vec::new();
        for input in inputs {
            let e = classify(&input, &ctx());
            assert_eq!(e.class, e.subclass.class(), "{input:?}");
            if !seen.contains(&e.subclass) {
                seen.push(e.subclass);
            }
        }
        let missing: Vec<_> = all_subclasses()
            .into_iter()
            .filter(|s| !seen.contains(s))
            .collect();
        assert!(missing.is_empty(), "unreachable: {missing:?}");
    }

    #[test]
    fn capacity_and_compute_gaps_differ_in_detail_and_fingerprint() {
        let gap = |gap| {
            classify(
                &FailureInput::CapabilityGap {
                    gap,
                    name: "gpu".into(),
                },
                &ctx(),
            )
        };
        let capacity = gap(GapKind::Capacity);
        let compute = gap(GapKind::Compute);
        assert_eq!(capacity.subclass, compute.subclass);
        assert_eq!(capacity.detail, "needs capacity: gpu");
        assert_ne!(capacity.fingerprint, compute.fingerprint);
        assert_eq!(gap_of(&capacity).unwrap().kind, GapKind::Capacity);
    }

    #[test]
    fn the_input_tool_and_the_worker_kind_feed_the_fingerprint() {
        let on = |tool_name: &str, ctx_tool, worker| {
            let input = FailureInput::ToolResult {
                tool: tool_name.into(),
                kind: ToolErrorKind::Timeout,
                message: "timed out after 30s".into(),
            };
            classify(
                &input,
                &Context {
                    tool: ctx_tool,
                    worker_kind: worker,
                },
            )
            .fingerprint
        };
        let local = Some(WorkerKind::Local);
        // The input's own tool wins; the context fills in when it is empty.
        assert_eq!(
            on("web_fetch", Some("other"), local),
            on("", Some("web_fetch"), local)
        );
        assert_ne!(on("web_fetch", None, local), on("web_search", None, local));
        assert_ne!(
            on("web_fetch", None, local),
            on("web_fetch", None, Some(WorkerKind::Peer))
        );
    }

    #[test]
    fn core_errors_map_to_precise_inputs() {
        use rustykrab_core::Error;
        use ErrorSubclass as S;
        let cases = [
            (
                Error::PendingApproval {
                    request_id: "r1".into(),
                    name: "carrier_login".into(),
                },
                S::Consent,
            ),
            (
                Error::ContextBudgetExceeded {
                    estimated_input_tokens: 9_000,
                    input_budget_tokens: 4_096,
                },
                S::Compute,
            ),
            (Error::ModelAuthError("bad key".into()), S::Credential),
            (Error::ContentPolicy, S::Refusal),
            (Error::ModelEmptyResponse("nothing".into()), S::Empty),
            (Error::ModelRateLimit("slow down".into()), S::UpstreamError),
            (Error::ModelOverloaded("busy".into()), S::UpstreamError),
            (Error::NotFound("item 7".into()), S::NotFound),
            (Error::ModelBadRequest("bad".into()), S::InvalidArgs),
            (
                Error::ToolExecution(rustykrab_core::ToolError::timeout("slow")),
                S::Timeout,
            ),
            (
                Error::Internal("the frobnicator sulked".into()),
                S::Unclassified,
            ),
        ];
        for (err, want) in cases {
            let input = FailureInput::from_core_error(Some("web_fetch"), &err);
            assert_eq!(sub(&input), want, "{err:?}");
        }
        let overflow = FailureInput::from_core_error(
            None,
            &Error::ContextBudgetExceeded {
                estimated_input_tokens: 9_000,
                input_budget_tokens: 4_096,
            },
        );
        let gap = gap_of(&classify(&overflow, &ctx())).unwrap();
        assert_eq!(gap.kind, GapKind::Capacity);
        assert_eq!(
            gap.subject,
            "context window: 9000 tokens needed, 4096 available"
        );
    }

    #[test]
    fn scenario_14_an_added_rule_classifies_the_failure_on_replay() {
        let evidence = raw("flux capacitor desynchronised at 88 mph");
        let before = classify(&evidence, &ctx());
        assert_eq!(before.class, ErrorClass::Unknown);
        assert_eq!(before.observed_by, UNCLASSIFIED);

        // What the `internal` item lands: one more row in the table.
        let mut rules = vec![MessageRule {
            name: "flux_capacitor",
            subclass: ErrorSubclass::Process,
            gap: None,
            patterns: &["flux capacitor desynchronised"],
        }];
        rules.extend_from_slice(MESSAGE_RULES);
        let after = classify_with(&rules, &evidence, &ctx());
        assert_eq!(after.subclass, ErrorSubclass::Process);
        assert_eq!(after.observed_by, "rule:flux_capacitor");
    }

    #[test]
    fn long_details_are_clipped() {
        let e = classify(&raw(&"x".repeat(2_000)), &ctx());
        assert_eq!(e.detail.chars().count(), DETAIL_MAX_CHARS + 3);
        assert!(e.detail.ends_with("..."));
    }
}
