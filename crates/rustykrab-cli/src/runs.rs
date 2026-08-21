//! Run inspection — read back the trace log written by [`crate::prompt_log`]
//! and reconstruct past agent executions.
//!
//! The daemon already records every model submission and response as a JSONL
//! row tagged with the run's `trace_id` (see
//! [`rustykrab_core::prompt_trace`]). This module is the reader: it groups
//! those rows by `trace_id` and renders them, so debugging a past run is a
//! command rather than a `jq` expression.
//!
//! Two subcommands:
//!
//! - `rustykrab-cli runs` — list recent runs, newest first.
//! - `rustykrab-cli run <trace-id>` — replay one run iteration by iteration.
//!
//! Both read `<data_dir>/logs/prompts.log*`, which only exists when the
//! daemon ran with `RUSTYKRAB_PROMPT_LOG=1`. Without it there is nothing to
//! read, and both commands say so rather than printing an empty table.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rustykrab_core::prompt_trace::TraceRecord;
use uuid::Uuid;

/// Default number of runs listed by `runs` when `--limit` is absent.
const DEFAULT_LIST_LIMIT: usize = 20;

/// Characters of message text shown per turn without `--full`.
const PREVIEW_CHARS: usize = 160;

/// One model call within a run: the submission and, when present, the
/// response it produced.
///
/// A prompt with no response means the call failed or the process died
/// mid-request — worth seeing rather than hiding, so the pairing is
/// deliberately optional.
#[derive(Debug, Default)]
struct Turn {
    prompt: Option<PromptRow>,
    response: Option<ResponseRow>,
}

#[derive(Debug)]
struct PromptRow {
    timestamp: DateTime<Utc>,
    message_count: usize,
    tool_names: Vec<String>,
}

#[derive(Debug)]
struct ResponseRow {
    text: Option<String>,
    tool_calls: Vec<String>,
    prompt_tokens: u32,
    completion_tokens: u32,
    stop_reason: String,
    duration_ms: u64,
    load_ms: Option<u64>,
    prompt_eval_ms: Option<u64>,
    eval_ms: Option<u64>,
}

/// A complete run: every model call sharing one `trace_id`.
#[derive(Debug)]
struct Run {
    trace_id: Uuid,
    provider: String,
    model: String,
    started_at: DateTime<Utc>,
    ended_at: DateTime<Utc>,
    turns: Vec<Turn>,
}

impl Run {
    fn iterations(&self) -> usize {
        self.turns.len()
    }

    fn tool_calls(&self) -> usize {
        self.turns
            .iter()
            .filter_map(|t| t.response.as_ref())
            .map(|r| r.tool_calls.len())
            .sum()
    }

    fn prompt_tokens(&self) -> u32 {
        self.turns
            .iter()
            .filter_map(|t| t.response.as_ref())
            .map(|r| r.prompt_tokens)
            .sum()
    }

    fn completion_tokens(&self) -> u32 {
        self.turns
            .iter()
            .filter_map(|t| t.response.as_ref())
            .map(|r| r.completion_tokens)
            .sum()
    }

    fn wall_ms(&self) -> u64 {
        self.turns
            .iter()
            .filter_map(|t| t.response.as_ref())
            .map(|r| r.duration_ms)
            .sum()
    }

    /// Summed server-reported phase durations. `None` when no response in
    /// the run carried timing — i.e. the provider doesn't report it.
    fn server_timing(&self) -> Option<(u64, u64, u64)> {
        let rows: Vec<&ResponseRow> = self
            .turns
            .iter()
            .filter_map(|t| t.response.as_ref())
            .filter(|r| r.prompt_eval_ms.is_some() || r.eval_ms.is_some() || r.load_ms.is_some())
            .collect();
        if rows.is_empty() {
            return None;
        }
        Some((
            rows.iter().filter_map(|r| r.load_ms).sum(),
            rows.iter().filter_map(|r| r.prompt_eval_ms).sum(),
            rows.iter().filter_map(|r| r.eval_ms).sum(),
        ))
    }

    /// The last response's stop reason — how the run ended.
    fn outcome(&self) -> &str {
        self.turns
            .iter()
            .rev()
            .find_map(|t| t.response.as_ref())
            .map(|r| r.stop_reason.as_str())
            .unwrap_or("incomplete")
    }
}

/// Outcome of scanning the trace log.
struct Scan {
    runs: Vec<Run>,
    /// Lines that failed to parse. Reported rather than hidden — a nonzero
    /// count means the log is truncated or was written by an incompatible
    /// build, and the runs shown may be missing turns.
    malformed: usize,
    files_read: usize,
}

/// Locate the rotated trace-log files, newest last.
///
/// `tracing_appender`'s daily rotation writes `prompts.log.YYYY-MM-DD`, so
/// the prefix match picks up every day's file.
fn trace_log_files(log_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(log_dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("prompts.log"))
        })
        .collect();
    files.sort();
    files
}

/// Parse every trace-log file into runs, newest run first.
fn scan(log_dir: &Path) -> Scan {
    let files = trace_log_files(log_dir);
    let files_read = files.len();
    let mut by_trace: BTreeMap<Uuid, Run> = BTreeMap::new();
    let mut malformed = 0usize;

    for path in files {
        let Ok(file) = File::open(&path) else {
            continue;
        };
        for line in BufReader::new(file).lines() {
            let Ok(line) = line else { continue };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<TraceRecord>(&line) {
                Ok(record) => absorb(&mut by_trace, record),
                Err(_) => malformed += 1,
            }
        }
    }

    let mut runs: Vec<Run> = by_trace.into_values().collect();
    runs.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    Scan {
        runs,
        malformed,
        files_read,
    }
}

/// Fold one trace row into the run it belongs to.
///
/// A response is attached to the last turn that has a prompt but no
/// response yet; that is the pairing the agent loop actually produces,
/// since a run issues one submission and awaits its reply before the next.
fn absorb(by_trace: &mut BTreeMap<Uuid, Run>, record: TraceRecord) {
    match record {
        TraceRecord::Prompt {
            trace_id,
            timestamp,
            provider,
            model,
            messages,
            tools,
            ..
        } => {
            let run = by_trace.entry(trace_id).or_insert_with(|| Run {
                trace_id,
                provider: provider.clone(),
                model: model.clone(),
                started_at: timestamp,
                ended_at: timestamp,
                turns: Vec::new(),
            });
            run.ended_at = run.ended_at.max(timestamp);
            run.started_at = run.started_at.min(timestamp);
            run.turns.push(Turn {
                prompt: Some(PromptRow {
                    timestamp,
                    message_count: messages.len(),
                    tool_names: tools.into_iter().map(|t| t.name).collect(),
                }),
                response: None,
            });
        }
        TraceRecord::Response {
            trace_id,
            timestamp,
            provider,
            model,
            message,
            prompt_tokens,
            completion_tokens,
            stop_reason,
            duration_ms,
            load_ms,
            prompt_eval_ms,
            eval_ms,
            ..
        } => {
            let run = by_trace.entry(trace_id).or_insert_with(|| Run {
                trace_id,
                provider,
                model,
                started_at: timestamp,
                ended_at: timestamp,
                turns: Vec::new(),
            });
            run.ended_at = run.ended_at.max(timestamp);
            let row = ResponseRow {
                text: message.content.as_text().map(str::to_owned),
                tool_calls: message
                    .content
                    .tool_calls()
                    .into_iter()
                    .map(|c| c.name.clone())
                    .collect(),
                prompt_tokens,
                completion_tokens,
                stop_reason,
                duration_ms,
                load_ms,
                prompt_eval_ms,
                eval_ms,
            };
            match run.turns.iter_mut().rev().find(|t| t.response.is_none()) {
                Some(turn) => turn.response = Some(row),
                // A response with no matching prompt — possible when the
                // log was rotated mid-run. Keep it as its own turn rather
                // than dropping evidence.
                None => run.turns.push(Turn {
                    prompt: None,
                    response: Some(row),
                }),
            }
        }
    }
}

/// Shorten text to a single-line preview.
fn preview(text: &str, limit: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= limit {
        return collapsed;
    }
    let truncated: String = collapsed.chars().take(limit).collect();
    format!("{truncated}…")
}

/// Render a duration in the most readable unit for its magnitude.
fn human_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}m{}s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

/// Message shown when the trace log is absent or empty.
fn explain_empty(log_dir: &Path, scan: &Scan) {
    if scan.files_read == 0 {
        println!("No trace log found in {}.", log_dir.display());
        println!();
        println!("Run the daemon with RUSTYKRAB_PROMPT_LOG=1 to record prompts,");
        println!("responses, and provider timing. It is off by default because");
        println!("the records can contain secrets.");
    } else {
        println!("Trace log found but no runs parsed from it.");
        if scan.malformed > 0 {
            println!("{} line(s) failed to parse.", scan.malformed);
        }
    }
}

/// `rustykrab-cli runs [--limit N] [--all]`
fn list(log_dir: &Path, args: &[String]) -> anyhow::Result<()> {
    let mut limit = DEFAULT_LIST_LIMIT;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--limit" | "-n" => {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow::anyhow!("--limit needs a number"))?;
                limit = value
                    .parse()
                    .map_err(|_| anyhow::anyhow!("--limit expects a number, got '{value}'"))?;
                i += 2;
            }
            "--all" => {
                limit = usize::MAX;
                i += 1;
            }
            other => anyhow::bail!("unknown option '{other}' (expected --limit N or --all)"),
        }
    }

    let scan = scan(log_dir);
    if scan.runs.is_empty() {
        explain_empty(log_dir, &scan);
        return Ok(());
    }

    println!(
        "{:<10}  {:<19}  {:>5}  {:>5}  {:>9}  {:>9}  {:<12}  MODEL",
        "RUN", "STARTED", "ITER", "TOOLS", "WALL", "PROMPT-EV", "OUTCOME"
    );
    for run in scan.runs.iter().take(limit) {
        let short = run.trace_id.to_string();
        let short = &short[..8];
        let prompt_eval = match run.server_timing() {
            Some((_, prompt_eval_ms, _)) => human_ms(prompt_eval_ms),
            None => "—".to_string(),
        };
        println!(
            "{:<10}  {:<19}  {:>5}  {:>5}  {:>9}  {:>9}  {:<12}  {}",
            short,
            run.started_at.format("%Y-%m-%d %H:%M:%S"),
            run.iterations(),
            run.tool_calls(),
            human_ms(run.wall_ms()),
            prompt_eval,
            run.outcome(),
            run.model,
        );
    }

    let shown = scan.runs.len().min(limit);
    println!();
    println!(
        "{shown} of {} run(s). Inspect one with: rustykrab-cli run <RUN>",
        scan.runs.len()
    );
    if scan.malformed > 0 {
        println!(
            "note: {} log line(s) failed to parse and were skipped.",
            scan.malformed
        );
    }
    Ok(())
}

/// `rustykrab-cli run <trace-id> [--full]`
fn show(log_dir: &Path, args: &[String]) -> anyhow::Result<()> {
    let mut wanted: Option<&str> = None;
    let mut full = false;
    for arg in args {
        match arg.as_str() {
            "--full" => full = true,
            other if other.starts_with('-') => {
                anyhow::bail!("unknown option '{other}' (expected --full)")
            }
            other => {
                if wanted.is_some() {
                    anyhow::bail!("expected a single run id, got an extra argument '{other}'");
                }
                wanted = Some(other);
            }
        }
    }
    let wanted = wanted
        .ok_or_else(|| anyhow::anyhow!("usage: rustykrab-cli run <RUN> [--full]"))?
        .to_lowercase();

    let scan = scan(log_dir);
    if scan.runs.is_empty() {
        explain_empty(log_dir, &scan);
        return Ok(());
    }

    // Prefix match so the short id from `runs` is enough to identify a run.
    let matches: Vec<&Run> = scan
        .runs
        .iter()
        .filter(|r| r.trace_id.to_string().starts_with(&wanted))
        .collect();
    let run = match matches.as_slice() {
        [] => anyhow::bail!("no run matching '{wanted}' — list them with: rustykrab-cli runs"),
        [only] => *only,
        many => {
            anyhow::bail!(
                "'{wanted}' matches {} runs — use more characters of the id",
                many.len()
            )
        }
    };

    let limit = if full { usize::MAX } else { PREVIEW_CHARS };

    println!("run      {}", run.trace_id);
    println!("provider {} · {}", run.provider, run.model);
    println!(
        "started  {}",
        run.started_at.format("%Y-%m-%d %H:%M:%S%.3f UTC")
    );
    println!(
        "spans    {} · {} iteration(s) · {} tool call(s)",
        human_ms((run.ended_at - run.started_at).num_milliseconds().max(0) as u64),
        run.iterations(),
        run.tool_calls()
    );
    println!("outcome  {}", run.outcome());
    println!();

    let mut previous_tools: Option<Vec<String>> = None;
    for (index, turn) in run.turns.iter().enumerate() {
        println!("── iteration {index} {}", "─".repeat(46));

        if let Some(prompt) = &turn.prompt {
            println!(
                "  → sent   {} message(s), {} tool schema(s)   {}",
                prompt.message_count,
                prompt.tool_names.len(),
                prompt.timestamp.format("%H:%M:%S%.3f")
            );
            // A change in the tool set is the event that breaks prefix-cache
            // reuse, so call it out explicitly rather than making the reader
            // diff two counts.
            if let Some(prev) = &previous_tools {
                let added: Vec<&String> = prompt
                    .tool_names
                    .iter()
                    .filter(|n| !prev.contains(n))
                    .collect();
                let removed: Vec<&String> = prev
                    .iter()
                    .filter(|n| !prompt.tool_names.contains(n))
                    .collect();
                if !added.is_empty() || !removed.is_empty() {
                    let mut parts = Vec::new();
                    for name in added {
                        parts.push(format!("+{name}"));
                    }
                    for name in removed {
                        parts.push(format!("-{name}"));
                    }
                    println!("    tool set changed: {}", parts.join(" "));
                }
            }
            if full {
                println!("    tools: {}", prompt.tool_names.join(", "));
            }
            previous_tools = Some(prompt.tool_names.clone());
        } else {
            println!("  → sent   (no matching prompt row — log rotated mid-run?)");
        }

        match &turn.response {
            Some(response) => {
                println!(
                    "  ← got    {} · {} in / {} out · {}",
                    response.stop_reason,
                    response.prompt_tokens,
                    response.completion_tokens,
                    human_ms(response.duration_ms),
                );
                if let (Some(load), Some(prompt_eval), Some(eval)) =
                    (response.load_ms, response.prompt_eval_ms, response.eval_ms)
                {
                    println!(
                        "    server:  prompt-eval {} · generate {} · load {}",
                        human_ms(prompt_eval),
                        human_ms(eval),
                        human_ms(load),
                    );
                    if load > 0 {
                        println!(
                            "    note:    non-zero load time — the model was reloaded for this call"
                        );
                    }
                }
                if !response.tool_calls.is_empty() {
                    println!("    calls:   {}", response.tool_calls.join(", "));
                }
                if let Some(text) = &response.text {
                    if !text.trim().is_empty() {
                        println!("    text:    {}", preview(text, limit));
                    }
                }
            }
            None => println!("  ← got    (no response recorded — call failed or process exited)"),
        }
        println!();
    }

    println!("── totals {}", "─".repeat(48));
    println!(
        "  tokens   {} in / {} out",
        run.prompt_tokens(),
        run.completion_tokens()
    );
    println!("  wall     {}", human_ms(run.wall_ms()));
    match run.server_timing() {
        Some((load, prompt_eval, eval)) => {
            let accounted = load + prompt_eval + eval;
            println!(
                "  server   prompt-eval {} · generate {} · load {}",
                human_ms(prompt_eval),
                human_ms(eval),
                human_ms(load),
            );
            if accounted > 0 {
                println!(
                    "  split    {:.0}% prompt-eval · {:.0}% generate · {:.0}% load",
                    prompt_eval as f64 * 100.0 / accounted as f64,
                    eval as f64 * 100.0 / accounted as f64,
                    load as f64 * 100.0 / accounted as f64,
                );
            }
        }
        None => println!("  server   (this provider reports no phase timing)"),
    }
    Ok(())
}

/// Entry point for the `runs` subcommand.
pub fn handle_list(data_dir: &Path, args: &[String]) -> anyhow::Result<()> {
    list(&data_dir.join("logs"), args)
}

/// Entry point for the `run` subcommand.
pub fn handle_show(data_dir: &Path, args: &[String]) -> anyhow::Result<()> {
    show(&data_dir.join("logs"), args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustykrab_core::model::{ProviderTiming, StopReason};
    use rustykrab_core::prompt_trace::TraceRecord;
    use rustykrab_core::types::{Message, MessageContent, Role, ToolCall, ToolSchema};
    use std::io::Write;

    fn message(text: &str) -> Message {
        Message {
            id: Uuid::new_v4(),
            role: Role::Assistant,
            content: MessageContent::Text(text.to_string()),
            created_at: Utc::now(),
        }
    }

    fn tool_call_message(name: &str) -> Message {
        Message {
            id: Uuid::new_v4(),
            role: Role::Assistant,
            content: MessageContent::ToolCall(ToolCall {
                id: "call-1".into(),
                name: name.into(),
                arguments: serde_json::json!({}),
            }),
            created_at: Utc::now(),
        }
    }

    fn schema(name: &str) -> ToolSchema {
        ToolSchema {
            name: name.to_string(),
            description: String::new(),
            parameters: serde_json::json!({}),
        }
    }

    fn prompt_row(trace_id: Uuid, tools: &[&str]) -> TraceRecord {
        TraceRecord::Prompt {
            trace_id,
            timestamp: Utc::now(),
            provider: "ollama".into(),
            model: "gemma4:26b".into(),
            streaming: false,
            messages: vec![message("hi")],
            tools: tools.iter().map(|n| schema(n)).collect(),
        }
    }

    fn response_row(
        trace_id: Uuid,
        message: Message,
        timing: Option<ProviderTiming>,
    ) -> TraceRecord {
        TraceRecord::Response {
            trace_id,
            timestamp: Utc::now(),
            provider: "ollama".into(),
            model: "gemma4:26b".into(),
            streaming: false,
            message,
            prompt_tokens: 100,
            completion_tokens: 20,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            stop_reason: format!("{:?}", StopReason::EndTurn),
            duration_ms: 500,
            server_total_ms: timing.map(|t| t.total_ms),
            load_ms: timing.map(|t| t.load_ms),
            prompt_eval_ms: timing.map(|t| t.prompt_eval_ms),
            eval_ms: timing.map(|t| t.eval_ms),
        }
    }

    /// Write records as JSONL into a temp dir laid out like the log dir.
    fn write_log(records: &[TraceRecord]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let mut file = File::create(dir.path().join("prompts.log.2026-08-21")).unwrap();
        for record in records {
            writeln!(file, "{}", serde_json::to_string(record).unwrap()).unwrap();
        }
        dir
    }

    #[test]
    fn scan_groups_rows_by_trace_id() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let dir = write_log(&[
            prompt_row(a, &["read"]),
            response_row(a, message("done"), None),
            prompt_row(b, &["read"]),
            response_row(b, message("also done"), None),
        ]);

        let scan = scan(dir.path());
        assert_eq!(scan.runs.len(), 2);
        assert_eq!(scan.malformed, 0);
        assert!(scan.runs.iter().all(|r| r.iterations() == 1));
    }

    #[test]
    fn response_pairs_with_its_prompt() {
        let id = Uuid::new_v4();
        let dir = write_log(&[
            prompt_row(id, &["read"]),
            response_row(id, tool_call_message("read"), None),
            prompt_row(id, &["read", "write"]),
            response_row(id, message("finished"), None),
        ]);

        let scan = scan(dir.path());
        let run = &scan.runs[0];
        assert_eq!(run.iterations(), 2);
        assert_eq!(run.tool_calls(), 1);
        // Every turn paired — no orphan rows appended.
        assert!(run
            .turns
            .iter()
            .all(|t| t.prompt.is_some() && t.response.is_some()));
    }

    #[test]
    fn server_timing_sums_only_rows_that_have_it() {
        let id = Uuid::new_v4();
        let timing = ProviderTiming {
            total_ms: 1000,
            load_ms: 200,
            prompt_eval_ms: 500,
            eval_ms: 300,
        };
        let dir = write_log(&[
            prompt_row(id, &["read"]),
            response_row(id, message("a"), Some(timing)),
            prompt_row(id, &["read"]),
            response_row(id, message("b"), None),
        ]);

        let scan = scan(dir.path());
        let (load, prompt_eval, eval) = scan.runs[0].server_timing().unwrap();
        assert_eq!((load, prompt_eval, eval), (200, 500, 300));
    }

    #[test]
    fn server_timing_absent_when_no_row_reports_it() {
        let id = Uuid::new_v4();
        let dir = write_log(&[
            prompt_row(id, &["read"]),
            response_row(id, message("a"), None),
        ]);
        assert!(scan(dir.path()).runs[0].server_timing().is_none());
    }

    #[test]
    fn malformed_lines_are_counted_not_fatal() {
        let id = Uuid::new_v4();
        let dir = tempfile::tempdir().unwrap();
        let mut file = File::create(dir.path().join("prompts.log")).unwrap();
        writeln!(file, "{{ not valid json").unwrap();
        writeln!(
            file,
            "{}",
            serde_json::to_string(&prompt_row(id, &["read"])).unwrap()
        )
        .unwrap();
        drop(file);

        let scan = scan(dir.path());
        assert_eq!(scan.malformed, 1);
        assert_eq!(scan.runs.len(), 1);
    }

    #[test]
    fn missing_log_dir_yields_empty_scan() {
        let scan = scan(Path::new("/nonexistent/rustykrab/logs"));
        assert!(scan.runs.is_empty());
        assert_eq!(scan.files_read, 0);
    }

    #[test]
    fn orphan_response_becomes_its_own_turn() {
        let id = Uuid::new_v4();
        let dir = write_log(&[response_row(id, message("orphan"), None)]);
        let scan = scan(dir.path());
        let run = &scan.runs[0];
        assert_eq!(run.turns.len(), 1);
        assert!(run.turns[0].prompt.is_none());
        assert!(run.turns[0].response.is_some());
    }

    #[test]
    fn preview_collapses_whitespace_and_truncates() {
        assert_eq!(preview("a\n  b\tc", 100), "a b c");
        assert_eq!(preview("abcdef", 3), "abc…");
    }

    #[test]
    fn human_ms_picks_a_readable_unit() {
        assert_eq!(human_ms(950), "950ms");
        assert_eq!(human_ms(1500), "1.5s");
        assert_eq!(human_ms(65_000), "1m5s");
    }

    #[test]
    fn trace_log_files_ignores_unrelated_files() {
        let dir = tempfile::tempdir().unwrap();
        File::create(dir.path().join("prompts.log.2026-08-20")).unwrap();
        File::create(dir.path().join("prompts.log.2026-08-21")).unwrap();
        File::create(dir.path().join("rustykrab.log")).unwrap();
        let files = trace_log_files(dir.path());
        assert_eq!(files.len(), 2);
    }
}
