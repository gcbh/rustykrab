//! The daemon's own log for one run's time window.
//!
//! The daemon writes `<data dir>/logs/rustykrab.log.<YYYY-MM-DD>`, one file
//! per UTC day (`tracing_appender::rolling::daily` in `rustykrab-cli`),
//! each line `<RFC 3339 timestamp> <LEVEL> <target>: <message> <fields>`.
//! [`slices`] reads the files the windows fall in once, and for each window
//! returns the lines stamped inside it (with a few seconds either side),
//! bounded in size and with secrets taken out ([`super::redact`]).
//!
//! When a window holds more than the bound, lines are kept in this order of
//! preference: those that mention one of the window's needles (the item id,
//! the run id), then `WARN` and `ERROR` lines, then the rest, latest first.
//! What is kept is written in the file's order, with a line saying how many
//! were left out wherever there is a gap. A line with no timestamp (a
//! multi-line message's continuation) goes with the line above it. Lines
//! are not assumed to be in time order: a non-blocking appender and a
//! restarted daemon both interleave.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDate, TimeDelta, Utc};
use serde::{Deserialize, Serialize};

use super::redact::redact;

/// The rolling log's file name prefix.
pub const LOG_PREFIX: &str = "rustykrab.log";

/// Bytes of log kept per window unless the caller says otherwise.
pub const DEFAULT_MAX_BYTES: usize = 64 * 1024;

/// Seconds either side of a run's window that still count as its log: the
/// lease is written a moment before the worker starts, and what the daemon
/// logs about judging the run comes just after the run's own end record.
const PAD_SECONDS: i64 = 2;

/// Days a single window may span; a longer one keeps its last days.
const MAX_DAYS: i64 = 3;

/// Characters kept of one log line.
const LINE_MAX: usize = 4_000;

/// One window to cut out of the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    /// What the caller knows the window by (a run id).
    pub key: String,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    /// Substrings that make a line worth keeping first: the item id, the
    /// run id, and their short forms.
    pub needles: Vec<String>,
}

/// What [`slices`] found for one window.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LogSlice {
    /// The kept lines, redacted, in the file's order.
    pub text: String,
    /// The window as searched, padding included.
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    /// The log files read for it.
    pub files: Vec<PathBuf>,
    /// Lines stamped inside the window.
    pub lines_in_window: usize,
    /// Lines of those that were kept.
    pub lines_kept: usize,
    /// Secrets taken out of the kept lines.
    pub redactions: usize,
    /// Whether the size bound left lines out.
    pub truncated: bool,
}

/// The file the daemon logged to on `day`.
pub fn log_file(dir: &Path, day: NaiveDate) -> PathBuf {
    dir.join(format!("{LOG_PREFIX}.{}", day.format("%Y-%m-%d")))
}

/// The timestamp a log line starts with, if it starts with one.
fn stamp(line: &str) -> Option<DateTime<Utc>> {
    let first = line.split_whitespace().next()?;
    DateTime::parse_from_rfc3339(first)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// Whether the line's level is `WARN` or `ERROR`.
fn is_alarm(line: &str) -> bool {
    matches!(line.split_whitespace().nth(1), Some("WARN" | "ERROR"))
}

struct Collected {
    window: Window,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    days: Vec<NaiveDate>,
    /// Each stamped line with its continuation lines.
    entries: Vec<String>,
}

/// Cut every window out of the log in `dir`, reading each day's file once.
/// A missing directory or file yields empty slices, not an error.
pub fn slices(dir: &Path, windows: &[Window], max_bytes: usize) -> BTreeMap<String, LogSlice> {
    let pad = TimeDelta::seconds(PAD_SECONDS);
    let mut collected: Vec<Collected> = windows
        .iter()
        .map(|w| {
            let to = w.to.max(w.from) + pad;
            let from = (w.from - pad).max(to - TimeDelta::days(MAX_DAYS));
            let mut days = Vec::new();
            let mut day = from.date_naive();
            while day <= to.date_naive() {
                days.push(day);
                day = day.succ_opt().unwrap_or(day);
                if days.len() > (MAX_DAYS + 1) as usize {
                    break;
                }
            }
            Collected {
                window: w.clone(),
                from,
                to,
                days,
                entries: Vec::new(),
            }
        })
        .collect();
    let mut all_days: Vec<NaiveDate> = collected.iter().flat_map(|c| c.days.clone()).collect();
    all_days.sort_unstable();
    all_days.dedup();
    for day in all_days {
        let Ok(file) = std::fs::File::open(log_file(dir, day)) else {
            continue;
        };
        // Which windows the last stamped line belonged to, so its
        // continuation lines go with it.
        let mut open: Vec<usize> = Vec::new();
        for line in BufReader::new(file).split(b'\n').map_while(Result::ok) {
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches('\r');
            match stamp(line) {
                Some(at) => {
                    open.clear();
                    for (i, c) in collected.iter_mut().enumerate() {
                        if c.days.contains(&day) && at >= c.from && at <= c.to {
                            c.entries.push(line.to_string());
                            open.push(i);
                        }
                    }
                }
                None => {
                    for i in &open {
                        if let Some(last) = collected[*i].entries.last_mut() {
                            last.push('\n');
                            last.push_str(line);
                        }
                    }
                }
            }
        }
    }
    collected
        .into_iter()
        .map(|c| {
            let files = c
                .days
                .iter()
                .map(|d| log_file(dir, *d))
                .filter(|p| p.is_file())
                .collect();
            let key = c.window.key.clone();
            (key, cut(c, files, max_bytes))
        })
        .collect()
}

/// Choose what fits in `max_bytes` and render it.
fn cut(c: Collected, files: Vec<PathBuf>, max_bytes: usize) -> LogSlice {
    let mut redactions = 0;
    let entries: Vec<String> = c
        .entries
        .iter()
        .map(|e| {
            let r = redact(e);
            redactions += r.count;
            match r.text.char_indices().nth(LINE_MAX) {
                Some((cut, _)) => format!("{}... [line cut]", &r.text[..cut]),
                None => r.text,
            }
        })
        .collect();
    let rank = |e: &str| -> u8 {
        if c.window
            .needles
            .iter()
            .any(|n| !n.is_empty() && e.contains(n.as_str()))
        {
            0
        } else if is_alarm(e) {
            1
        } else {
            2
        }
    };
    // Best rank first, and within a rank the latest first: what is nearest
    // the failure matters most.
    let mut order: Vec<usize> = (0..entries.len()).collect();
    order.sort_by_key(|i| (rank(&entries[*i]), std::cmp::Reverse(*i)));
    let mut keep = vec![false; entries.len()];
    let mut used = 0usize;
    for i in order {
        let cost = entries[i].len() + 1;
        if used + cost > max_bytes {
            continue;
        }
        used += cost;
        keep[i] = true;
    }
    let kept = keep.iter().filter(|k| **k).count();
    let mut text = String::with_capacity(used + 64);
    let mut gap = 0usize;
    for (i, entry) in entries.iter().enumerate() {
        if !keep[i] {
            gap += 1;
            continue;
        }
        if gap > 0 {
            text.push_str(&format!(
                "[{gap} line(s) left out to fit {max_bytes} bytes]\n"
            ));
            gap = 0;
        }
        text.push_str(entry);
        text.push('\n');
    }
    if gap > 0 {
        text.push_str(&format!(
            "[{gap} line(s) left out to fit {max_bytes} bytes]\n"
        ));
    }
    LogSlice {
        text,
        from: Some(c.from),
        to: Some(c.to),
        files,
        lines_in_window: entries.len(),
        lines_kept: kept,
        redactions,
        truncated: kept < entries.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn window(key: &str, from: &str, to: &str, needles: &[&str]) -> Window {
        Window {
            key: key.into(),
            from: at(from),
            to: at(to),
            needles: needles.iter().map(|n| n.to_string()).collect(),
        }
    }

    const FAKE: &str = "7000000001:AAFakeFakeFakeFakeFakeFakeFakeFake_0";

    fn write(dir: &Path, day: &str, lines: &[String]) {
        std::fs::write(
            dir.join(format!("{LOG_PREFIX}.{day}")),
            lines.join("\n") + "\n",
        )
        .unwrap();
    }

    #[test]
    fn a_window_keeps_its_lines_and_redacts_the_bot_token() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "2026-10-13",
            &[
                "2026-10-13T10:00:00.000000Z  INFO rustykrab_cli: before the run".to_string(),
                "2026-10-13T10:05:01.000000Z  INFO rustykrab_agent: external worker run ended item=item-1 ok=false".to_string(),
                format!("2026-10-13T10:05:02.000000Z ERROR rustykrab_channels::telegram: Telegram getUpdates network error: error sending request for url (https://api.telegram.org/bot{FAKE}/getUpdates?timeout=30) consecutive_errors=1"),
                "  a continuation line with no timestamp".to_string(),
                "2026-10-13T10:30:00.000000Z  INFO rustykrab_cli: long after the run".to_string(),
            ],
        );
        let got = slices(
            dir.path(),
            &[window(
                "run-1",
                "2026-10-13T10:05:00Z",
                "2026-10-13T10:05:03Z",
                &["item-1"],
            )],
            DEFAULT_MAX_BYTES,
        );
        let slice = &got["run-1"];
        assert_eq!(slice.lines_in_window, 2);
        assert_eq!(slice.lines_kept, 2);
        assert!(!slice.truncated);
        assert_eq!(slice.redactions, 1);
        assert!(!slice.text.contains("AAFake"), "{}", slice.text);
        assert!(
            slice
                .text
                .contains("https://api.telegram.org/bot<redacted>/getUpdates"),
            "{}",
            slice.text
        );
        assert!(slice.text.contains("a continuation line"), "{}", slice.text);
        assert!(!slice.text.contains("before the run"));
        assert!(!slice.text.contains("long after the run"));
        assert_eq!(slice.files.len(), 1);
    }

    #[test]
    fn a_window_over_the_bound_prefers_its_item_then_warnings_then_the_latest() {
        let dir = tempfile::tempdir().unwrap();
        let mut lines = Vec::new();
        for n in 0..40 {
            lines.push(format!(
                "2026-10-13T10:00:{n:02}.000000Z  INFO rustykrab_cli: routine line number {n} with some padding to make it long"
            ));
        }
        lines[3] =
            "2026-10-13T10:00:03.000000Z  INFO rustykrab_control: leased item=abcd1234 to pinch"
                .into();
        lines[7] =
            "2026-10-13T10:00:07.000000Z  WARN rustykrab_control: something unrelated went wrong"
                .into();
        write(dir.path(), "2026-10-13", &lines);
        let got = slices(
            dir.path(),
            &[window(
                "r",
                "2026-10-13T10:00:00Z",
                "2026-10-13T10:00:39Z",
                &["abcd1234"],
            )],
            400,
        );
        let slice = &got["r"];
        assert_eq!(slice.lines_in_window, 40);
        assert!(slice.truncated);
        assert!(slice.lines_kept < 40);
        assert!(slice.text.contains("item=abcd1234"), "{}", slice.text);
        assert!(slice.text.contains("something unrelated"), "{}", slice.text);
        assert!(
            slice.text.contains("routine line number 39"),
            "{}",
            slice.text
        );
        assert!(
            !slice.text.contains("routine line number 10 "),
            "{}",
            slice.text
        );
        assert!(slice.text.contains("line(s) left out"), "{}", slice.text);
        // Kept lines stay in the file's order.
        let item = slice.text.find("item=abcd1234").unwrap();
        let warn = slice.text.find("something unrelated").unwrap();
        let last = slice.text.find("routine line number 39").unwrap();
        assert!(item < warn && warn < last);
    }

    #[test]
    fn windows_across_midnight_read_both_days_and_a_missing_log_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "2026-10-13",
            &["2026-10-13T23:59:58.000000Z  INFO a: late".to_string()],
        );
        write(
            dir.path(),
            "2026-10-14",
            &["2026-10-14T00:00:01.000000Z  INFO a: early".to_string()],
        );
        let got = slices(
            dir.path(),
            &[
                window("night", "2026-10-13T23:59:57Z", "2026-10-14T00:00:02Z", &[]),
                window("none", "2026-09-01T00:00:00Z", "2026-09-01T00:01:00Z", &[]),
            ],
            DEFAULT_MAX_BYTES,
        );
        assert_eq!(got["night"].lines_kept, 2);
        assert_eq!(got["night"].files.len(), 2);
        assert_eq!(
            got["none"],
            LogSlice {
                from: got["none"].from,
                to: got["none"].to,
                ..LogSlice::default()
            }
        );
        let missing = slices(
            &dir.path().join("no-such-dir"),
            &[window(
                "x",
                "2026-10-13T00:00:00Z",
                "2026-10-13T00:00:01Z",
                &[],
            )],
            DEFAULT_MAX_BYTES,
        );
        assert_eq!(missing["x"].lines_in_window, 0);
    }

    #[test]
    fn lines_out_of_time_order_are_still_found() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "2026-10-13",
            &[
                "2026-10-13T12:00:00.000000Z  INFO a: later line written first".to_string(),
                "2026-10-13T10:00:01.000000Z ERROR a: the line in the window".to_string(),
            ],
        );
        let got = slices(
            dir.path(),
            &[window(
                "r",
                "2026-10-13T10:00:00Z",
                "2026-10-13T10:00:02Z",
                &[],
            )],
            DEFAULT_MAX_BYTES,
        );
        assert_eq!(got["r"].lines_kept, 1);
        assert!(got["r"].text.contains("the line in the window"));
    }
}
