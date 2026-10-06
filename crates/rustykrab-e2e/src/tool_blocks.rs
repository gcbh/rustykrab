//! The tool blocks a conversation's requests declared, read from the
//! daemon's log.
//!
//! Every provider request logs one `tool block sent` line carrying the
//! conversation id and a fingerprint of the tools array it declared
//! (`rustykrab_core::tool_block`). The plan's rule that a run's tool block
//! is fixed from turn 0 (section 12, scenario 10) is checked by reading
//! those lines back: one fingerprint for the whole conversation means no
//! request re-rendered the front of the prompt.

use std::path::Path;

/// One request's tool block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolBlockSeen {
    pub fingerprint: String,
    pub num_tools: usize,
}

/// Remove ANSI escape sequences: the daemon's stdout log layer may colour
/// field names, which would otherwise split `key=value` pairs.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// The value of `key=` in a formatted log line, up to the next space.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!(" {key}=");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let end = rest.find(' ').unwrap_or(rest.len());
    Some(rest[..end].trim_matches('"'))
}

/// The tool blocks `conversation_id`'s requests declared, oldest first,
/// out of daemon log text.
pub fn parse(log: &str, conversation_id: &str) -> Vec<ToolBlockSeen> {
    log.lines()
        .filter(|line| line.contains("tool block sent"))
        .map(strip_ansi)
        .filter(|line| field(line, "conversation_id") == Some(conversation_id))
        .filter_map(|line| {
            Some(ToolBlockSeen {
                fingerprint: field(&line, "tool_block")?.to_string(),
                num_tools: field(&line, "num_tools")?.parse().ok()?,
            })
        })
        .collect()
}

/// [`parse`] over the daemon log in `data_dir`; empty when there is none.
pub fn read(data_dir: &Path, conversation_id: &str) -> Vec<ToolBlockSeen> {
    std::fs::read_to_string(data_dir.join("daemon.log"))
        .map(|log| parse(&log, conversation_id))
        .unwrap_or_default()
}

/// Why a conversation's tool block was not fixed, or `None` when it was:
/// at least `min_requests` requests, all with one fingerprint.
pub fn unchanged(blocks: &[ToolBlockSeen], min_requests: usize) -> Option<String> {
    if blocks.len() < min_requests {
        return Some(format!(
            "{} request(s) logged a tool block, expected at least {min_requests}",
            blocks.len()
        ));
    }
    let first = blocks.first()?;
    let changes: Vec<String> = blocks
        .iter()
        .enumerate()
        .filter(|(_, b)| b.fingerprint != first.fingerprint)
        .map(|(i, b)| format!("request {i}: {} ({} tools)", b.fingerprint, b.num_tools))
        .collect();
    if changes.is_empty() {
        None
    } else {
        Some(format!(
            "the tool block changed after request 0 ({}, {} tools): {}",
            first.fingerprint,
            first.num_tools,
            changes.join("; ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "11111111-1111-1111-1111-111111111111";
    const B: &str = "22222222-2222-2222-2222-222222222222";

    fn line(conv: &str, print: &str, n: usize, ansi: bool) -> String {
        if ansi {
            format!(
                "2026-09-27T10:00:00Z \u{1b}[32m INFO\u{1b}[0m \u{1b}[2mrustykrab_providers::tool_block\u{1b}[0m\u{1b}[2m:\u{1b}[0m tool block sent \u{1b}[3mprovider\u{1b}[0m\u{1b}[2m=\u{1b}[0m\"ollama\" \u{1b}[3mconversation_id\u{1b}[0m\u{1b}[2m=\u{1b}[0m{conv} \u{1b}[3mtrace_id\u{1b}[0m\u{1b}[2m=\u{1b}[0m- \u{1b}[3mtool_block\u{1b}[0m\u{1b}[2m=\u{1b}[0m{print} \u{1b}[3mnum_tools\u{1b}[0m\u{1b}[2m=\u{1b}[0m{n} \u{1b}[3mtool_tokens\u{1b}[0m\u{1b}[2m=\u{1b}[0m0 \u{1b}[3mchanged\u{1b}[0m\u{1b}[2m=\u{1b}[0mfalse"
            )
        } else {
            format!(
                "2026-09-27T10:00:00Z  INFO rustykrab_providers::tool_block: tool block sent provider=\"scripted\" conversation_id={conv} trace_id=- tool_block={print} num_tools={n} tool_tokens=0 changed=false"
            )
        }
    }

    #[test]
    fn blocks_are_read_per_conversation_with_or_without_colour() {
        let log = [
            line(A, "aaaa", 13, true),
            line(B, "bbbb", 4, false),
            "2026-09-27 INFO something else conversation_id=x".to_string(),
            line(A, "aaaa", 13, false),
        ]
        .join("\n");
        let a = parse(&log, A);
        let seen = ToolBlockSeen {
            fingerprint: "aaaa".into(),
            num_tools: 13,
        };
        assert_eq!(a, [seen.clone(), seen]);
        assert_eq!(parse(&log, B).len(), 1);
        assert!(unchanged(&a, 2).is_none());
        assert!(unchanged(&a, 3).unwrap().contains("expected at least 3"));
    }

    #[test]
    fn a_change_names_the_request_that_made_it() {
        let log = [line(A, "aaaa", 13, false), line(A, "cccc", 14, false)].join("\n");
        let why = unchanged(&parse(&log, A), 1).unwrap();
        assert!(why.contains("request 1: cccc (14 tools)"), "{why}");
    }
}
