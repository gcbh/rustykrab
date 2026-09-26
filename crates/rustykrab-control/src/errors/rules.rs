//! The ordered message rule table.
//!
//! A message is matched against the rules top to bottom and the first rule
//! with a matching pattern wins, so a specific rule sits above a general one
//! ("command not found" above "not found"). Patterns match the lowercased
//! message: text patterns as substrings, all-digit patterns (HTTP status
//! codes) as whole numbers and only in a message that says `http`, `status`
//! or `response`, so "processed 400 items" is not a bad request.
//!
//! This table is the extension point section 9 asks for: an `internal` item
//! filed for an `unknown` error lands as one more row here, with a test that
//! replays the evidence it was filed with.

use rustykrab_core::work::ErrorSubclass;

use super::GapKind;

/// One row of the table: the subclass a message means when it contains any
/// of `patterns`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageRule {
    /// Recorded as `observed_by: rule:<name>`.
    pub name: &'static str,
    pub subclass: ErrorSubclass,
    /// For a rule that finds a missing capability, the gap it names, so the
    /// ladder's order 2 knows what to acquire, build or request.
    pub gap: Option<GapKind>,
    pub patterns: &'static [&'static str],
}

impl MessageRule {
    /// Whether any pattern matches `lowered` (the message, lowercased).
    pub fn matches(&self, lowered: &str) -> bool {
        self.patterns.iter().any(|p| pattern_matches(lowered, p))
    }
}

/// Words that make a bare number in a message read as an HTTP status.
const STATUS_CONTEXT: &[&str] = &["http", "status", "response"];

fn pattern_matches(hay: &str, pattern: &str) -> bool {
    if !pattern.is_empty() && pattern.bytes().all(|b| b.is_ascii_digit()) {
        STATUS_CONTEXT.iter().any(|w| hay.contains(w)) && contains_number(hay, pattern)
    } else {
        hay.contains(pattern)
    }
}

/// Whether `number` occurs in `hay` as a whole number, not inside a longer
/// run of letters or digits.
fn contains_number(hay: &str, number: &str) -> bool {
    let bytes = hay.as_bytes();
    hay.match_indices(number).any(|(at, _)| {
        let before = at.checked_sub(1).map(|i| bytes[i]);
        let after = bytes.get(at + number.len()).copied();
        !before.is_some_and(|b| b.is_ascii_alphanumeric())
            && !after.is_some_and(|b| b.is_ascii_alphanumeric())
    })
}

const fn rule(
    name: &'static str,
    subclass: ErrorSubclass,
    gap: Option<GapKind>,
    patterns: &'static [&'static str],
) -> MessageRule {
    MessageRule {
        name,
        subclass,
        gap,
        patterns,
    }
}

/// The rules, most specific first.
pub const MESSAGE_RULES: &[MessageRule] = &[
    rule(
        "hallucinated_tool",
        ErrorSubclass::HallucinatedTool,
        None,
        &[
            "unknown tool",
            "no such tool",
            "tool not found",
            "not a registered tool",
            "no tool named",
        ],
    ),
    rule(
        "consent",
        ErrorSubclass::Consent,
        Some(GapKind::Consent),
        &[
            "needs your approval",
            "requires approval",
            "pending approval",
            "awaiting approval",
            "consent required",
            "requires consent",
        ],
    ),
    rule(
        "credential",
        ErrorSubclass::Credential,
        Some(GapKind::Credential),
        &[
            "401",
            "unauthorized",
            "unauthenticated",
            "not authenticated",
            "authentication failed",
            "authentication required",
            "invalid api key",
            "missing api key",
            "no api key",
            "api key not",
            "invalid token",
            "token expired",
            "expired token",
            "token has expired",
            "missing credential",
            "no credential",
            "credential not found",
            "invalid credentials",
            "login required",
            "not logged in",
        ],
    ),
    rule(
        "tool_unavailable",
        ErrorSubclass::ToolGap,
        Some(GapKind::Tool),
        &[
            "not configured",
            "tool is not loaded",
            "tool not loaded",
            "server is not connected",
            "mcp server unavailable",
        ],
    ),
    rule(
        "capacity",
        ErrorSubclass::Compute,
        Some(GapKind::Capacity),
        &[
            "context length",
            "context window",
            "maximum context",
            "too many tokens",
            "prompt is too long",
            "out of memory",
            "cannot allocate memory",
            "enomem",
        ],
    ),
    rule(
        "disk",
        ErrorSubclass::Disk,
        None,
        &[
            "no space left",
            "enospc",
            "disk full",
            "disk quota",
            "edquot",
            "read-only file system",
            "erofs",
        ],
    ),
    rule(
        "dependency",
        ErrorSubclass::Dependency,
        Some(GapKind::Install),
        &[
            "command not found",
            "not installed",
            "no module named",
            "cannot find module",
            "module not found",
            "modulenotfounderror",
            "library not loaded",
            "cannot open shared object",
            "executable file not found",
            "missing dependency",
        ],
    ),
    rule(
        "rate_limit",
        ErrorSubclass::UpstreamError,
        None,
        &[
            "429",
            "529",
            "rate limit",
            "rate-limit",
            "ratelimit",
            "too many requests",
            "quota exceeded",
            "overloaded",
        ],
    ),
    rule(
        "upstream_5xx",
        ErrorSubclass::UpstreamError,
        None,
        &[
            "500",
            "502",
            "503",
            "504",
            "internal server error",
            "bad gateway",
            "service unavailable",
            "gateway timeout",
            "temporarily unavailable",
        ],
    ),
    rule(
        "timeout",
        ErrorSubclass::Timeout,
        None,
        &["timed out", "timeout", "deadline exceeded", "etimedout"],
    ),
    rule(
        "network",
        ErrorSubclass::Network,
        None,
        &[
            "connection refused",
            "econnrefused",
            "connection reset",
            "econnreset",
            "connection aborted",
            "connection closed",
            "broken pipe",
            "network is unreachable",
            "enetunreach",
            "no route to host",
            "ehostunreach",
            "name resolution",
            "could not resolve host",
            "failed to lookup address",
            "getaddrinfo",
            "dns error",
            "tls handshake",
            "network error",
        ],
    ),
    rule(
        "permission",
        ErrorSubclass::Permission,
        None,
        &[
            "403",
            "permission denied",
            "eacces",
            "eperm",
            "operation not permitted",
            "forbidden",
            "access denied",
            "access is denied",
        ],
    ),
    rule(
        "invalid_args",
        ErrorSubclass::InvalidArgs,
        None,
        &[
            "400",
            "invalid json",
            "json parse",
            "expected value at line",
            "eof while parsing",
            "unexpected end of json",
            "failed to parse",
            "missing field",
            "unknown field",
            "invalid type:",
            "invalid argument",
            "invalid input",
            "bad request",
            "einval",
        ],
    ),
    rule(
        "not_found",
        ErrorSubclass::NotFound,
        None,
        &[
            "404",
            "enoent",
            "no such file",
            "not found",
            "does not exist",
        ],
    ),
    rule(
        "refusal",
        ErrorSubclass::Refusal,
        None,
        &[
            "content policy",
            "refused to respond",
            "i can't help with",
            "i cannot help with",
            "i can't assist",
            "i cannot assist",
        ],
    ),
    rule(
        "loop",
        ErrorSubclass::Loop,
        None,
        &[
            "loop detected",
            "stuck in a loop",
            "repeated the same tool call",
        ],
    ),
    rule(
        "empty",
        ErrorSubclass::Empty,
        None,
        &["empty response", "empty completion", "no content returned"],
    ),
    rule(
        "process",
        ErrorSubclass::Process,
        None,
        &[
            "segmentation fault",
            "sigsegv",
            "panicked at",
            "core dumped",
            "killed by signal",
            "abort trap",
            "exit status",
            "exited with",
            "non-zero exit",
        ],
    ),
];

/// The first rule, in table order, that matches `message` and whose subclass
/// `allowed` accepts.
pub(crate) fn first_match<'r>(
    rules: &'r [MessageRule],
    message: &str,
    allowed: impl Fn(ErrorSubclass) -> bool,
) -> Option<&'r MessageRule> {
    let lowered = message.to_lowercase();
    rules
        .iter()
        .find(|r| allowed(r.subclass) && r.matches(&lowered))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subclass_of(message: &str) -> Option<ErrorSubclass> {
        first_match(MESSAGE_RULES, message, |_| true).map(|r| r.subclass)
    }

    #[test]
    fn common_patterns_classify() {
        use ErrorSubclass::*;
        let cases = [
            ("request timed out after 30s", Timeout),
            ("Deadline Exceeded", Timeout),
            ("connect: Connection refused (os error 61)", Network),
            ("HTTP 429 Too Many Requests", UpstreamError),
            ("status 503", UpstreamError),
            ("ENOENT: no such file or directory, open 'a.txt'", NotFound),
            ("open /etc/shadow: Permission denied", Permission),
            (
                "invalid JSON: expected value at line 1 column 1",
                InvalidArgs,
            ),
            ("write failed: No space left on device", Disk),
            ("bash: jq: command not found", Dependency),
            ("401 Unauthorized", Credential),
            ("HTTP 401", Credential),
            ("unknown tool 'web_serch'", HallucinatedTool),
            ("MCP server 'github' is not configured", ToolGap),
            (
                "prompt is too long: 250000 tokens > 200000 maximum",
                Compute,
            ),
            ("thread 'main' panicked at src/main.rs:4:5", Process),
        ];
        for (message, want) in cases {
            assert_eq!(subclass_of(message), Some(want), "{message}");
        }
    }

    #[test]
    fn specific_rules_win_over_general_ones() {
        // "command not found" is a missing dependency, not a missing file.
        assert_eq!(
            subclass_of("sh: rg: command not found"),
            Some(ErrorSubclass::Dependency)
        );
        // A 504 is the upstream's failure, not a local timeout.
        assert_eq!(
            subclass_of("HTTP 504 Gateway Timeout"),
            Some(ErrorSubclass::UpstreamError)
        );
        // A 403 carrying a bad key is a credential gap.
        assert_eq!(
            subclass_of("403 Forbidden: invalid API key"),
            Some(ErrorSubclass::Credential)
        );
    }

    #[test]
    fn status_codes_need_a_status_context_and_whole_numbers() {
        assert_eq!(subclass_of("processed 400 items then stopped"), None);
        assert_eq!(subclass_of("http status 4000"), None);
        assert_eq!(subclass_of("http status 14290"), None);
        assert_eq!(
            subclass_of("response: 400"),
            Some(ErrorSubclass::InvalidArgs)
        );
        assert_eq!(subclass_of("gibberish"), None);
    }

    #[test]
    fn rule_names_are_unique_and_gaps_match_their_subclass() {
        let mut names: Vec<_> = MESSAGE_RULES.iter().map(|r| r.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), MESSAGE_RULES.len());
        for r in MESSAGE_RULES {
            if let Some(gap) = r.gap {
                assert_eq!(gap.subclass(), r.subclass, "{}", r.name);
            }
            assert!(!r.patterns.is_empty(), "{}", r.name);
            for p in r.patterns {
                assert_eq!(p.to_lowercase(), *p, "{}: patterns are lowercase", r.name);
            }
        }
    }
}
