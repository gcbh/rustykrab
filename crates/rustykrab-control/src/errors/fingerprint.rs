//! Fingerprints and recurrence (section 9: "Fingerprints drive recurrence").
//!
//! A fingerprint is the first 16 hex characters of a SHA-256 over the class,
//! subclass, tool, worker kind and the normalised message. Normalising
//! lowercases the message, collapses whitespace and replaces the parts that
//! change from one occurrence to the next (UUIDs, hex and mixed ids, numbers,
//! paths, URLs, quoted strings) with placeholders, so the same failure on
//! another item or in another run shares a fingerprint.

use std::collections::BTreeMap;

use rustykrab_core::work::{ErrorClass, ErrorSubclass, WorkerKind};
use sha2::{Digest, Sha256};

/// How many times a fingerprint is seen before the ladder stops repairing
/// the symptom and files the improvement (order 3).
pub const DEFAULT_PROMOTE_THRESHOLD: u32 = 3;

const FINGERPRINT_LEN: usize = 16;

/// The stable fingerprint of a failure.
pub fn fingerprint(
    class: ErrorClass,
    subclass: ErrorSubclass,
    tool: Option<&str>,
    worker_kind: Option<WorkerKind>,
    message: &str,
) -> String {
    let mut hasher = Sha256::new();
    for part in [
        class.as_str(),
        subclass.as_str(),
        tool.unwrap_or_default(),
        worker_kind.map(|w| w.as_str()).unwrap_or_default(),
        &normalise(message),
    ] {
        hasher.update(part.as_bytes());
        // A separator that cannot occur in the parts keeps ("ab", "c") and
        // ("a", "bc") apart.
        hasher.update(b"\x1f");
    }
    let mut hex = hex::encode(hasher.finalize());
    hex.truncate(FINGERPRINT_LEN);
    hex
}

/// The message with its variable parts replaced by placeholders: `<url>`,
/// `<path>`, `<str>` (a quoted string), `<uuid>`, `<id>` (a hex or mixed
/// letter-and-digit id) and `<n>` (a number).
pub fn normalise(message: &str) -> String {
    let unquoted = replace_quoted(&message.to_lowercase());
    unquoted
        .split_whitespace()
        .map(normalise_word)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Replace `'...'`, `"..."` and `` `...` `` with `<str>`. A single quote only
/// opens at the start of a word and closes at the end of one, so the
/// apostrophe in "can't" is left alone.
fn replace_quoted(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let opens = match c {
            '"' | '`' => true,
            '\'' => i == 0 || !chars[i - 1].is_alphanumeric(),
            _ => false,
        };
        if opens {
            let close = (i + 1..chars.len()).find(|&j| {
                chars[j] == c
                    && (c != '\'' || chars.get(j + 1).is_none_or(|n| !n.is_alphanumeric()))
            });
            if let Some(j) = close {
                out.push_str("<str>");
                i = j + 1;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

const LEADING_PUNCT: &[char] = &['(', '[', '{', '<', '"', '\''];
const TRAILING_PUNCT: &[char] = &['.', ',', ';', ':', '!', '?', ')', ']', '}', '>', '"', '\''];

fn normalise_word(word: &str) -> String {
    // Placeholders already written by `replace_quoted` stay as they are.
    if word.contains("<str>") && word.trim_matches(|c: char| !c.is_alphanumeric()) == "str" {
        return word.to_string();
    }
    let core = word.trim_start_matches(LEADING_PUNCT);
    let lead = &word[..word.len() - core.len()];
    let trimmed = core.trim_end_matches(TRAILING_PUNCT);
    let trail = &core[trimmed.len()..];
    let core = trimmed;
    let replaced = if core.contains("://") {
        "<url>".to_string()
    } else if is_path(core) {
        "<path>".to_string()
    } else {
        replace_ids(core)
    };
    format!("{lead}{replaced}{trail}")
}

fn is_path(word: &str) -> bool {
    if word.len() < 2 {
        return false;
    }
    let b = word.as_bytes();
    if word.starts_with('/')
        || word.starts_with("~/")
        || word.starts_with("./")
        || word.starts_with("../")
        || word.contains('\\')
        || (b.len() > 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'/')
    {
        return true;
    }
    // `a/b/c`, or `dir/file.ext`; a single slash between words
    // ("input/output", "application/json") is prose.
    let slashes = word.matches('/').count();
    slashes >= 2
        || (slashes == 1
            && word
                .rsplit('/')
                .next()
                .is_some_and(|last| last.contains('.')))
}

/// Replace UUIDs, ids and numbers inside one word, leaving the rest.
fn replace_ids(word: &str) -> String {
    let mut out = String::with_capacity(word.len());
    let mut rest = word;
    while !rest.is_empty() {
        if is_uuid_prefix(rest) {
            out.push_str("<uuid>");
            rest = &rest[36..];
            continue;
        }
        let run_len = rest
            .find(|c: char| !c.is_ascii_alphanumeric())
            .unwrap_or(rest.len());
        if run_len == 0 {
            let c = rest.chars().next().expect("rest is not empty");
            out.push(c);
            rest = &rest[c.len_utf8()..];
            continue;
        }
        out.push_str(&replace_run(&rest[..run_len]));
        rest = &rest[run_len..];
    }
    out
}

fn is_uuid_prefix(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 36
        && (0..36).all(|i| match i {
            8 | 13 | 18 | 23 => b[i] == b'-',
            _ => b[i].is_ascii_hexdigit(),
        })
        && b.get(36).is_none_or(|c| !c.is_ascii_alphanumeric())
}

/// One run of ASCII letters and digits.
fn replace_run(run: &str) -> String {
    let has_digit = run.bytes().any(|b| b.is_ascii_digit());
    let has_alpha = run.bytes().any(|b| b.is_ascii_alphabetic());
    let all_hex = run.bytes().all(|b| b.is_ascii_hexdigit());
    let hex_after_0x = run
        .strip_prefix("0x")
        .is_some_and(|h| !h.is_empty() && h.bytes().all(|b| b.is_ascii_hexdigit()));
    if hex_after_0x || (has_digit && has_alpha && ((all_hex && run.len() >= 6) || run.len() >= 8)) {
        return "<id>".to_string();
    }
    if !has_digit {
        return run.to_string();
    }
    // Digits inside a word ("sha256", "item42") become `<n>`.
    let mut out = String::with_capacity(run.len());
    let mut in_number = false;
    for c in run.chars() {
        if c.is_ascii_digit() {
            if !in_number {
                out.push_str("<n>");
                in_number = true;
            }
        } else {
            out.push(c);
            in_number = false;
        }
    }
    out
}

/// Counts of each fingerprint seen, which the controller feeds as failures
/// are classified. A count at or above `promote_threshold` promotes the
/// failure from order 1 to order 3: file the improvement rather than repair
/// the symptom again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recurrence {
    counts: BTreeMap<String, u32>,
    pub promote_threshold: u32,
}

impl Default for Recurrence {
    fn default() -> Self {
        Recurrence {
            counts: BTreeMap::new(),
            promote_threshold: DEFAULT_PROMOTE_THRESHOLD,
        }
    }
}

impl Recurrence {
    pub fn new() -> Self {
        Recurrence::default()
    }

    pub fn with_threshold(promote_threshold: u32) -> Self {
        Recurrence {
            promote_threshold,
            ..Recurrence::default()
        }
    }

    /// Record one occurrence and return the new count.
    pub fn observe(&mut self, fingerprint: &str) -> u32 {
        let n = self.counts.entry(fingerprint.to_string()).or_insert(0);
        *n = n.saturating_add(1);
        *n
    }

    /// How many times `fingerprint` has been seen.
    pub fn count(&self, fingerprint: &str) -> u32 {
        self.counts.get(fingerprint).copied().unwrap_or(0)
    }

    /// Whether `fingerprint` has recurred often enough to promote.
    pub fn promotes(&self, fingerprint: &str) -> bool {
        self.count(fingerprint) >= self.promote_threshold.max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(subclass: ErrorSubclass, message: &str) -> String {
        fingerprint(
            subclass.class(),
            subclass,
            Some("web_fetch"),
            Some(WorkerKind::Local),
            message,
        )
    }

    #[test]
    fn normalising_replaces_the_variable_parts() {
        assert_eq!(
            normalise(
                "Item 3f2a9c1e-0b7d-4c1a-9e2f-5a6b7c8d9e0f failed after 42 retries at \
                 /tmp/run-17/out.log (see https://example.com/runs/17?x=1): 'boom'"
            ),
            "item <uuid> failed after <n> retries at <path> (see <url>): <str>"
        );
        assert_eq!(
            normalise("commit a1b2c3d4e5 on msg_01AbCdEf23GhIj, code 0x1F, sha256"),
            "commit <id> on msg_<id>, code <id>, sha<n>"
        );
        assert_eq!(
            normalise("can't   open  input/output\tfor src/main.rs:12"),
            "can't open input/output for <path>"
        );
        assert_eq!(normalise("  "), "");
    }

    #[test]
    fn fingerprints_are_short_stable_hex() {
        let a = fp(ErrorSubclass::Timeout, "request timed out");
        assert_eq!(a.len(), 16);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(a, fp(ErrorSubclass::Timeout, "request timed out"));
    }

    #[test]
    fn a_changed_id_number_or_path_keeps_the_fingerprint() {
        let base = fp(
            ErrorSubclass::NotFound,
            "item 3f2a9c1e-0b7d-4c1a-9e2f-5a6b7c8d9e0f: 404 for /data/a/1.json after 3 tries",
        );
        let changed = fp(
            ErrorSubclass::NotFound,
            "Item 99999999-aaaa-4bbb-8ccc-dddddddddddd: 404 for /srv/x/77.json after 12 tries",
        );
        assert_eq!(base, changed);
        assert_eq!(
            fp(
                ErrorSubclass::NotFound,
                "fetch https://a.example/1 failed: 'x1'"
            ),
            fp(
                ErrorSubclass::NotFound,
                "fetch http://b.example/2?q=3 failed: \"y2\""
            )
        );
        assert_eq!(
            fp(ErrorSubclass::CheckFailed, "commit deadbeef12 differs"),
            fp(ErrorSubclass::CheckFailed, "commit 0a1b2c3d4e differs")
        );
    }

    #[test]
    fn a_changed_subclass_tool_worker_or_wording_changes_the_fingerprint() {
        let base = fp(ErrorSubclass::Timeout, "request failed");
        assert_ne!(base, fp(ErrorSubclass::Network, "request failed"));
        assert_ne!(base, fp(ErrorSubclass::Timeout, "request refused"));
        let other_tool = fingerprint(
            ErrorClass::Tool,
            ErrorSubclass::Timeout,
            Some("web_search"),
            Some(WorkerKind::Local),
            "request failed",
        );
        assert_ne!(base, other_tool);
        let other_worker = fingerprint(
            ErrorClass::Tool,
            ErrorSubclass::Timeout,
            Some("web_fetch"),
            Some(WorkerKind::ClaudeCode),
            "request failed",
        );
        assert_ne!(base, other_worker);
        let no_tool = fingerprint(
            ErrorClass::Tool,
            ErrorSubclass::Timeout,
            None,
            Some(WorkerKind::Local),
            "request failed",
        );
        assert_ne!(base, no_tool);
    }

    #[test]
    fn recurrence_counts_and_promotes_at_the_threshold() {
        let mut r = Recurrence::new();
        assert_eq!(r.promote_threshold, DEFAULT_PROMOTE_THRESHOLD);
        assert_eq!(r.count("f"), 0);
        assert_eq!(r.observe("f"), 1);
        assert_eq!(r.observe("f"), 2);
        assert!(!r.promotes("f"));
        assert_eq!(r.observe("f"), 3);
        assert!(r.promotes("f"));
        assert_eq!(r.count("g"), 0);
        assert!(!r.promotes("g"));

        let mut eager = Recurrence::with_threshold(1);
        eager.observe("f");
        assert!(eager.promotes("f"));
        // A zero threshold still needs one occurrence.
        assert!(!Recurrence::with_threshold(0).promotes("never-seen"));
    }
}
