//! Bounded diagnostic observations from one owned native child capture.
//!
//! This is not operation proof. In particular Claude's request is not an exit,
//! old log files are not trusted inputs, and a completed command is not a goal.
use std::collections::{HashMap, HashSet};

use rustykrab_core::work::WorkerKind;
use serde::Serialize;
use serde_json::Value;

const INPUT_MAX: usize = 4 * 1024 * 1024;
const LINE_MAX: usize = 256 * 1024;
const RECORDS_MAX: usize = 32;
const TEXT_MAX: usize = 1024;
const ID_MAX: usize = 256;

#[derive(Debug, Serialize)]
struct Text {
    text: String,
    original_bytes: usize,
    truncated: bool,
}

impl Text {
    fn new(value: &str) -> Self {
        let mut end = value.len().min(TEXT_MAX);
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        Self {
            text: value[..end].into(),
            original_bytes: value.len(),
            truncated: end < value.len(),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", content = "reason", rename_all = "snake_case")]
enum Outcome {
    Requested,
    Completed,
    Failed,
    Interrupted,
    Unobserved(&'static str),
}

#[derive(Debug, Serialize)]
struct Observation {
    session: Option<String>,
    correlation: Option<String>,
    /// Runtime's original command display, not rewritten shell text.
    command: Option<Text>,
    /// Existing decoder unwraps only exact known-shell argv; otherwise the
    /// display is unchanged. This field is diagnostic, not canonical proof.
    decoded_command: Option<Text>,
    outcome: Outcome,
    runtime_status: Option<Text>,
    exit_code: Option<i64>,
    stdout: Option<Text>,
    stderr: Option<Text>,
    result_text: Option<Text>,
    #[serde(skip)]
    ambiguous: bool,
}

#[derive(Debug, Default, Serialize)]
struct Omissions {
    input_bytes: usize,
    oversized_lines: usize,
    malformed_lines: usize,
    records: usize,
}

#[derive(Debug, Serialize)]
pub(super) struct Observations {
    schema_version: u8,
    /// Never a replacement for controller-owned operation_probe evidence.
    diagnostic_only: bool,
    capture_incomplete: bool,
    observations: Vec<Observation>,
    omissions: Omissions,
}

type Key = (Option<String>, String);

struct Reader {
    batch: Observations,
    session: Option<String>,
    calls: HashMap<Key, usize>,
    terminals: HashSet<Key>,
}

impl Reader {
    fn identity(value: &Value) -> Option<String> {
        value
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= ID_MAX)
            .map(str::to_owned)
    }

    fn command(value: &Value) -> Option<String> {
        match value {
            Value::String(s) => Some(s.clone()),
            Value::Array(parts) if parts.iter().all(Value::is_string) => Some(value.to_string()),
            _ => None,
        }
    }

    fn add(&mut self, id: Option<String>, command: &Value, start: bool) -> Option<usize> {
        let key = id.as_ref().map(|id| (self.session.clone(), id.clone()));
        if let Some(index) = key.as_ref().and_then(|key| self.calls.get(key)).copied() {
            if start {
                self.invalidate(index, "duplicate_request_id");
            }
            return Some(index);
        }
        if self.batch.observations.len() == RECORDS_MAX {
            self.batch.omissions.records += 1;
            self.batch.capture_incomplete = true;
            return None;
        }
        let index = self.batch.observations.len();
        let command = Self::command(command).map(|c| Text::new(&c));
        let outcome = if id.is_none() {
            Outcome::Unobserved("missing_or_oversized_correlation_id")
        } else if !start {
            Outcome::Unobserved("missing_request")
        } else if command.as_ref().is_none_or(|c| c.truncated) {
            Outcome::Unobserved("missing_or_truncated_command")
        } else {
            Outcome::Requested
        };
        let ambiguous = outcome != Outcome::Requested;
        self.batch.observations.push(Observation {
            session: self.session.clone(),
            correlation: id,
            command,
            decoded_command: None,
            outcome,
            runtime_status: None,
            exit_code: None,
            stdout: None,
            stderr: None,
            result_text: None,
            ambiguous,
        });
        if let Some(key) = key {
            self.calls.insert(key, index);
        }
        Some(index)
    }

    fn invalidate(&mut self, index: usize, reason: &'static str) {
        let record = &mut self.batch.observations[index];
        record.ambiguous = true;
        record.outcome = Outcome::Unobserved(reason);
    }

    fn terminal(&mut self, index: usize) {
        let record = &self.batch.observations[index];
        if let Some(id) = &record.correlation {
            if !self.terminals.insert((record.session.clone(), id.clone())) {
                self.invalidate(index, "duplicate_terminal_id");
            }
        }
    }

    fn claude(&mut self, event: &Value) {
        if let Some(session) = Self::identity(&event["session_id"]) {
            self.session = Some(session);
        } else if !event["session_id"].is_null() {
            self.session = None;
            self.batch.capture_incomplete = true;
        }
        let Some(blocks) = event["message"]["content"].as_array() else {
            return;
        };
        let results = blocks.iter().filter(|b| b["type"] == "tool_result").count();
        for block in blocks {
            if event["type"] == "assistant"
                && block["type"] == "tool_use"
                && block["name"] == "Bash"
            {
                self.add(
                    Self::identity(&block["id"]),
                    &block["input"]["command"],
                    true,
                );
            } else if event["type"] == "user" && block["type"] == "tool_result" {
                let Some(id) = Self::identity(&block["tool_use_id"]) else {
                    continue;
                };
                let Some(index) = self.calls.get(&(self.session.clone(), id)).copied() else {
                    // Results of other tools must not be mistaken for Bash.
                    continue;
                };
                self.terminal(index);
                let record = &mut self.batch.observations[index];
                if record.ambiguous {
                    continue;
                }
                // The raw result belongs to a single result block only. Its
                // absence or a multi-result message cannot confer exit semantics.
                let raw = if results == 1 {
                    &event["tool_use_result"]
                } else {
                    &Value::Null
                };
                record.stdout = raw["stdout"].as_str().map(Text::new);
                record.stderr = raw["stderr"].as_str().map(Text::new);
                record.result_text = block["content"].as_str().map(Text::new);
                record.outcome = if raw["interrupted"] == true {
                    Outcome::Interrupted
                } else if block["is_error"] == true {
                    Outcome::Failed
                } else {
                    // No numeric exit contract has been established for Claude.
                    Outcome::Unobserved("claude_exit_semantics_unestablished")
                };
            }
        }
    }

    fn codex(&mut self, event: &Value) {
        if event["type"] == "thread.started" {
            self.session = Self::identity(&event["thread_id"]);
            if self.session.is_none() {
                self.batch.capture_incomplete = true;
            }
        }
        let item = &event["item"];
        if item["type"] == "command_execution" {
            let started = event["type"] == "item.started";
            if !started && event["type"] != "item.completed" {
                return;
            }
            let Some(index) = self.add(Self::identity(&item["id"]), &item["command"], started)
            else {
                return;
            };
            let decoded = crate::external_worker::codex_command(&item["command"]);
            if started {
                self.batch.observations[index].decoded_command = decoded.as_deref().map(Text::new);
                return;
            }
            self.terminal(index);
            let record = &mut self.batch.observations[index];
            record.runtime_status = item["status"].as_str().map(Text::new);
            record.exit_code = item["exit_code"].as_i64();
            record.stdout = item["aggregated_output"].as_str().map(Text::new);
            if record.ambiguous {
                return;
            }
            if Self::command(&item["command"]).as_deref()
                != record.command.as_ref().map(|c| c.text.as_str())
            {
                self.invalidate(index, "terminal_command_mismatch");
                return;
            }
            let record = &mut self.batch.observations[index];
            record.outcome = match (item["status"].as_str(), record.exit_code) {
                (Some("completed"), Some(0)) => Outcome::Completed,
                (Some("failed"), Some(code)) if code != 0 => Outcome::Failed,
                (Some("completed"), Some(code)) if code != 0 => Outcome::Failed,
                (Some("failed"), Some(0)) => Outcome::Unobserved("contradictory_status_and_exit"),
                _ => Outcome::Unobserved("missing_or_nonterminal_status_or_exit"),
            };
        } else if event["msg"]["type"] == "exec_command_begin" {
            // Legacy begin events are requests, not completion evidence.
            let msg = &event["msg"];
            if let Some(index) = self.add(Self::identity(&msg["call_id"]), &msg["command"], true) {
                // Do not pair a legacy request with an unrelated modern result.
                self.batch.observations[index].ambiguous = true;
            }
        }
    }
}

/// Called only with stdout captured from an owned child, never a saved log.
pub(super) fn observe(kind: WorkerKind, stdout: &[u8], capture_complete: bool) -> Observations {
    let mut reader = Reader {
        batch: Observations {
            schema_version: 1,
            diagnostic_only: true,
            capture_incomplete: !capture_complete || stdout.len() > INPUT_MAX,
            observations: Vec::new(),
            omissions: Omissions {
                input_bytes: stdout.len().saturating_sub(INPUT_MAX),
                ..Omissions::default()
            },
        },
        session: None,
        calls: HashMap::new(),
        terminals: HashSet::new(),
    };
    for line in stdout[..stdout.len().min(INPUT_MAX)].split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        if line.len() > LINE_MAX {
            reader.batch.omissions.oversized_lines += 1;
            reader.batch.capture_incomplete = true;
            continue;
        }
        let Ok(event) = serde_json::from_slice::<Value>(line) else {
            reader.batch.omissions.malformed_lines += 1;
            reader.batch.capture_incomplete = true;
            continue;
        };
        if !event.is_object() {
            reader.batch.omissions.malformed_lines += 1;
            reader.batch.capture_incomplete = true;
            continue;
        }
        match kind {
            WorkerKind::ClaudeCode => reader.claude(&event),
            WorkerKind::Codex => reader.codex(&event),
            _ => {}
        }
    }
    if reader.batch.capture_incomplete {
        for record in &mut reader.batch.observations {
            if record.outcome == Outcome::Completed {
                record.outcome = Outcome::Unobserved("incomplete_capture");
            }
        }
    }
    reader.batch
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn batch(kind: WorkerKind, events: &[Value]) -> Observations {
        let text = events
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        observe(kind, text.as_bytes(), true)
    }

    fn command(typ: &str, id: &str, command: &str) -> Value {
        json!({"type": typ, "item": {"type":"command_execution", "id":id,
            "command":command,"status":"completed","exit_code":0,"aggregated_output":"node v1.2.3"}})
    }

    fn pair() -> Vec<Value> {
        vec![
            command("item.started", "a", "node --version"),
            command("item.completed", "a", "node --version"),
        ]
    }

    #[test]
    fn codex_correlation_requires_terminal_numeric_outcome() {
        assert_eq!(
            batch(WorkerKind::Codex, &pair()).observations[0].outcome,
            Outcome::Completed
        );
        let mut events = pair();
        events[1]["item"]["exit_code"] = json!(7);
        events[1]["item"]["status"] = json!("failed");
        let b = batch(WorkerKind::Codex, &events);
        assert_eq!(b.observations[0].outcome, Outcome::Failed);
        assert_eq!(b.observations[0].exit_code, Some(7));
        for (status, exit) in [
            (json!("in_progress"), json!(0)),
            (Value::Null, json!(0)),
            (json!("completed"), Value::Null),
            (json!("completed"), json!(false)),
            (json!("failed"), json!(0)),
        ] {
            events[1]["item"]["status"] = status;
            events[1]["item"]["exit_code"] = exit;
            assert!(matches!(
                batch(WorkerKind::Codex, &events).observations[0].outcome,
                Outcome::Unobserved(_)
            ));
        }
    }

    #[test]
    fn missing_duplicate_reordered_and_changed_commands_are_not_completion() {
        assert_eq!(
            batch(WorkerKind::Codex, &pair()[..1]).observations[0].outcome,
            Outcome::Requested
        );
        let mut variants = vec![
            vec![pair()[1].clone()],
            vec![pair()[1].clone(), pair()[0].clone()],
            vec![pair()[0].clone(), pair()[1].clone(), pair()[1].clone()],
            vec![pair()[0].clone(), pair()[0].clone(), pair()[1].clone()],
        ];
        let mut changed = pair();
        changed[1]["item"]["command"] = json!("node --version; echo forged");
        variants.push(changed);
        for events in variants {
            assert!(matches!(
                batch(WorkerKind::Codex, &events).observations[0].outcome,
                Outcome::Unobserved(_)
            ));
        }
    }

    #[test]
    fn invocation_and_thread_boundaries_do_not_pair() {
        let first = batch(WorkerKind::Codex, &pair()[..1]);
        let second = batch(WorkerKind::Codex, &pair()[1..]);
        assert_eq!(first.observations[0].outcome, Outcome::Requested);
        assert!(matches!(
            second.observations[0].outcome,
            Outcome::Unobserved(_)
        ));
        let events = vec![
            json!({"type":"thread.started","thread_id":"one"}),
            pair()[0].clone(),
            json!({"type":"thread.started","thread_id":"two"}),
            pair()[1].clone(),
        ];
        assert!(batch(WorkerKind::Codex, &events)
            .observations
            .iter()
            .all(|r| r.outcome != Outcome::Completed));
    }

    fn bash() -> Value {
        json!({"type":"assistant","session_id":"s", "message":{"content":[
            {"type":"tool_use","name":"Bash","id":"a","input":{"command":"node --version"}}]}})
    }

    fn result() -> Value {
        json!({"type":"user","session_id":"s","message":{"content":[
            {"type":"tool_result","tool_use_id":"a","is_error":false,"content":"not exit proof"}]},
            "tool_use_result":{"stdout":"node v1.2.3","stderr":"","interrupted":false,"exit_code":0}})
    }

    #[test]
    fn claude_request_success_prose_and_unconfirmed_exit_do_not_prove_completion() {
        let b = batch(WorkerKind::ClaudeCode, &[bash(), result()]);
        assert_eq!(
            b.observations[0].outcome,
            Outcome::Unobserved("claude_exit_semantics_unestablished")
        );
        assert_eq!(b.observations[0].exit_code, None);
        assert_eq!(
            b.observations[0].stdout.as_ref().unwrap().text,
            "node v1.2.3"
        );
        let mut failed = result();
        failed["message"]["content"][0]["is_error"] = json!(true);
        failed["message"]["content"][0]["content"] = json!("permission denied by runtime");
        let denied = batch(WorkerKind::ClaudeCode, &[bash(), failed.clone()]);
        assert_eq!(
            denied.observations[0].result_text.as_ref().unwrap().text,
            "permission denied by runtime"
        );
        assert_eq!(
            batch(WorkerKind::ClaudeCode, &[bash(), failed]).observations[0].outcome,
            Outcome::Failed
        );
        let mut stopped = result();
        stopped["tool_use_result"]["interrupted"] = json!(true);
        assert_eq!(
            batch(WorkerKind::ClaudeCode, &[bash(), stopped]).observations[0].outcome,
            Outcome::Interrupted
        );
    }

    #[test]
    fn claude_duplicates_other_tools_and_session_changes_cannot_supply_result() {
        let duplicate = batch(WorkerKind::ClaudeCode, &[bash(), result(), result()]);
        assert_eq!(
            duplicate.observations[0].outcome,
            Outcome::Unobserved("duplicate_terminal_id")
        );
        let mut other = result();
        other["message"]["content"][0]["tool_use_id"] = json!("other-tool");
        assert_eq!(
            batch(WorkerKind::ClaudeCode, &[bash(), other]).observations[0].outcome,
            Outcome::Requested
        );
        let mut switched = result();
        switched["session_id"] = json!("other-session");
        assert_eq!(
            batch(WorkerKind::ClaudeCode, &[bash(), switched]).observations[0].outcome,
            Outcome::Requested
        );
    }

    #[test]
    fn nested_model_json_legacy_requests_and_incomplete_capture_do_not_complete() {
        let fake = json!({"type":"assistant","message":{"content":[{"type":"text","text":pair()[1].to_string()}]}});
        assert!(batch(WorkerKind::ClaudeCode, &[fake])
            .observations
            .is_empty());
        let legacy =
            json!({"msg":{"type":"exec_command_begin","call_id":"l","command":"node --version"}});
        assert_eq!(
            batch(WorkerKind::Codex, &[legacy]).observations[0].outcome,
            Outcome::Requested
        );
        let text = pair()
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n{malformed";
        let b = observe(WorkerKind::Codex, text.as_bytes(), true);
        assert!(b.capture_incomplete);
        assert_eq!(
            b.observations[0].outcome,
            Outcome::Unobserved("incomplete_capture")
        );
        let text = pair()
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        let partial = observe(WorkerKind::Codex, text.as_bytes(), false);
        assert!(partial.capture_incomplete);
        assert_eq!(
            partial.observations[0].outcome,
            Outcome::Unobserved("incomplete_capture")
        );
    }

    #[test]
    fn bounds_are_explicit_and_unicode_safe() {
        let mut events = pair();
        events[1]["item"]["aggregated_output"] = json!("🦀".repeat(1000));
        let b = batch(WorkerKind::Codex, &events);
        let text = b.observations[0].stdout.as_ref().unwrap();
        assert!(text.truncated && text.text.len() <= TEXT_MAX);
        assert_eq!(text.original_bytes, 4000);
        let events = (0..RECORDS_MAX + 1)
            .map(|n| command("item.started", &n.to_string(), "node --version"))
            .collect::<Vec<_>>();
        let b = batch(WorkerKind::Codex, &events);
        assert_eq!(b.observations.len(), RECORDS_MAX);
        assert_eq!(b.omissions.records, 1);
        assert!(b.capture_incomplete && b.diagnostic_only);
        let b = observe(WorkerKind::Codex, &vec![b'x'; INPUT_MAX + 10], true);
        assert_eq!(b.omissions.input_bytes, 10);
        assert!(b.capture_incomplete && b.omissions.oversized_lines > 0);
    }

    #[test]
    fn invalid_identities_and_commands_do_not_correlate_a_completion() {
        for id in [Value::Null, json!("x".repeat(ID_MAX + 1))] {
            let mut events = pair();
            events[0]["item"]["id"] = id.clone();
            events[1]["item"]["id"] = id;
            assert!(batch(WorkerKind::Codex, &events)
                .observations
                .iter()
                .all(|r| r.outcome != Outcome::Completed));
        }
        for value in [json!("x".repeat(TEXT_MAX + 1)), json!([42]), Value::Null] {
            let mut events = pair();
            events[0]["item"]["command"] = value.clone();
            events[1]["item"]["command"] = value;
            assert!(batch(WorkerKind::Codex, &events)
                .observations
                .iter()
                .all(|r| r.outcome != Outcome::Completed));
        }
    }
}
