//! `result_report`: the typed result contract that ends a worker run (plan
//! section 5), the typed successor of `task_complete`'s free-text summary.
//!
//! Every field is a claim the controller verifies before anything counts.
//! The host checks what code can check here (no cascade reason as a block,
//! an error subclass that belongs to its class, at most one `supersedes`
//! target per draft) and hands the report to the [`WorkBackend`]. The
//! `discovered` drafts travel inside the report and are never filed from
//! here: the controller validates them together as one graph (section 6.5).
//!
//! **Ending the run.** A successful call returns `{"ok": true, "summary"}`,
//! the shape `task_complete` returns, and [`run_end_summary`] reads it for
//! either tool, so the runner can end the run on the call's success with no
//! further model turn. The runner recognises `task_complete` by name before
//! it executes; `result_report` is read after it executes, because a
//! rejected report must not end the run (see `finalize_run_end` in
//! `rustykrab-agent`). [`Tool::blocks_turn`] additionally makes a text-only
//! reply end the turn instead of re-prompting for `task_complete`.

use std::collections::HashSet;
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::work::{
    BlockedReason, BlockedReport, ErrorClass, ErrorSubclass, Question, RejectionReason,
    ResultReport, WorkError, BLOCKED_SHAPE_GUIDANCE,
};
use rustykrab_core::{validate_tool_args, Error, Result, Tool, ToolError};
use serde_json::{json, Map, Value};

use crate::work_backend::{host_provenance, with_work_run, WorkBackend};
use crate::work_file::{
    check_keys, draft_properties, parse_draft, take_artifacts, take_artifacts_of, take_opt_text,
    take_text, take_text_list, DraftMode, Problem, ARTIFACT_KINDS, ENTRY_MAX, LIST_MAX, NAME_MAX,
    POINTER_MAX,
};

/// Tools whose successful result ends the run, with its `summary` as the
/// final message.
pub const RUN_ENDING_TOOLS: [&str; 2] = ["task_complete", "result_report"];

/// The final message a successful run-ending call leaves: `Some(summary)`
/// when `tool_name` is one of [`RUN_ENDING_TOOLS`] and `output` is its
/// success shape `{"ok": true, "summary": "..."}`.
pub fn run_end_summary(tool_name: &str, output: &Value) -> Option<String> {
    if !RUN_ENDING_TOOLS.contains(&tool_name) || output.get("ok") != Some(&Value::Bool(true)) {
        return None;
    }
    output
        .get("summary")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The calls that end a worker run on success: its report, the planner's
/// accepted `work_plan` (its output is one accepted graph, plan section
/// 6.1), and a question or capability request that parked the item (section
/// 7). Each succeeds with `{"ok": true, "summary"}`; a rejected plan or an
/// answered question does not carry it, so the run goes on.
pub const WORKER_RUN_ENDING_TOOLS: [&str; 4] = [
    "result_report",
    "work_plan",
    "ask_user",
    "capability_request",
];

/// [`run_end_summary`] for a worker run: `Some(summary)` when `tool_name` is
/// one of [`WORKER_RUN_ENDING_TOOLS`] and `output` is the success shape.
pub fn worker_run_end_summary(tool_name: &str, output: &Value) -> Option<String> {
    if !WORKER_RUN_ENDING_TOOLS.contains(&tool_name) || output.get("ok") != Some(&Value::Bool(true))
    {
        return None;
    }
    output
        .get("summary")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

const SUMMARY_MAX: usize = 1_500;
const FULL_REPORT_MAX: usize = 32_000;
const DETAIL_MAX: usize = 600;
const ARTIFACTS_MAX: usize = 20;

/// What a result's `artifacts` may point at: a draft's pointer kinds, plus
/// `classifier_rule`, the `<subclass>: <pattern>` row an `internal` item
/// lands for an `unknown` failure (plan section 9). The controller replays
/// the failure through it before it counts.
const RESULT_ARTIFACT_KINDS: [&str; 7] = [
    ARTIFACT_KINDS[0],
    ARTIFACT_KINDS[1],
    ARTIFACT_KINDS[2],
    ARTIFACT_KINDS[3],
    ARTIFACT_KINDS[4],
    ARTIFACT_KINDS[5],
    "classifier_rule",
];
const CHANGED_PATHS_MAX: usize = 100;
const CHECKS_MAX: usize = 20;
const QUESTIONS_MAX: usize = 5;
const OPTIONS_MAX: usize = 6;
const OPTION_MAX: usize = 120;
const DISCOVERED_MAX: usize = 6;

/// Recorded as the error's `observed_by`: the worker classified it itself.
/// The fingerprint is left empty for the controller, which owns it.
const OBSERVED_BY: &str = "result_report";

const REPORT_KEYS: [&str; 11] = [
    "item",
    "summary",
    "artifacts",
    "changed_paths",
    "commit",
    "checks_run",
    "known_limits",
    "blocked",
    "error",
    "questions",
    "discovered",
];

const CLASSES: [ErrorClass; 8] = [
    ErrorClass::Tool,
    ErrorClass::Model,
    ErrorClass::CapabilityGap,
    ErrorClass::Environment,
    ErrorClass::Verification,
    ErrorClass::Policy,
    ErrorClass::Budget,
    ErrorClass::Unknown,
];

const SUBCLASSES: [ErrorSubclass; 30] = {
    use ErrorSubclass::*;
    [
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
    ]
};

fn parse_class(raw: &str) -> Option<ErrorClass> {
    CLASSES.iter().copied().find(|c| c.as_str() == raw)
}

/// Every subclass has one string form, shared by serde and `as_str`
/// (`tool` under `capability_gap` included).
fn parse_subclass(raw: &str) -> Option<ErrorSubclass> {
    ErrorSubclass::parse(raw)
}

fn subclasses_of(class: ErrorClass) -> String {
    SUBCLASSES
        .iter()
        .filter(|s| s.class() == class)
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Reasons a worker may report, the same list [`BLOCKED_SHAPE_GUIDANCE`]
/// names; the rest are the controller's.
fn reportable_reasons() -> Vec<&'static str> {
    BlockedReason::MODEL_FILEABLE
        .iter()
        .map(|r| r.as_str())
        .collect()
}

fn parse_blocked(v: &Value, out: &mut Vec<Problem>) -> Option<BlockedReport> {
    let Some(obj) = v.as_object() else {
        out.push(Problem::invalid(
            "",
            "blocked",
            "must be an object {reason, detail, needs}",
        ));
        return None;
    };
    let at = "blocked.";
    check_keys(obj, &["reason", "detail", "needs"], at, out);
    let raw = take_text(obj, "reason", at, NAME_MAX, true, out);
    let detail = take_text(obj, "detail", at, DETAIL_MAX, false, out);
    let needs = take_text_list(obj, "needs", at, LIST_MAX, ENTRY_MAX, out);
    let reason = match BlockedReason::parse(&raw) {
        Some(r) if r.is_cascade() => {
            out.push(Problem::invalid(
                at,
                "reason",
                format!(
                    "`{raw}` is set only by the controller's cascade; report what blocks you: {}",
                    reportable_reasons().join(", ")
                ),
            ));
            return None;
        }
        Some(r) if !r.is_model_fileable() => {
            out.push(Problem::invalid(
                at,
                "reason",
                format!(
                    "`{raw}` is set only by the controller; report what blocks you: {}",
                    reportable_reasons().join(", ")
                ),
            ));
            return None;
        }
        Some(r) => r,
        None if raw.is_empty() => return None,
        None => {
            out.push(Problem::invalid(
                at,
                "reason",
                format!("`{raw}` is not one of {}", reportable_reasons().join(", ")),
            ));
            return None;
        }
    };
    Some(BlockedReport {
        reason,
        detail,
        needs,
    })
}

fn parse_error(v: &Value, out: &mut Vec<Problem>) -> Option<WorkError> {
    let Some(obj) = v.as_object() else {
        out.push(Problem::invalid(
            "",
            "error",
            "must be an object {class, subclass, detail}",
        ));
        return None;
    };
    let at = "error.";
    check_keys(
        obj,
        &["class", "subclass", "detail", "artifact_refs"],
        at,
        out,
    );
    let raw_class = take_text(obj, "class", at, NAME_MAX, true, out);
    let raw_sub = take_text(obj, "subclass", at, NAME_MAX, true, out);
    let detail = take_text(obj, "detail", at, DETAIL_MAX, true, out);
    let artifact_refs = take_artifacts(obj, "artifact_refs", at, LIST_MAX, out);

    let class = parse_class(&raw_class);
    if class.is_none() && !raw_class.is_empty() {
        out.push(Problem::invalid(
            at,
            "class",
            format!(
                "`{raw_class}` is not one of {}",
                CLASSES.map(|c| c.as_str()).join(", ")
            ),
        ));
    }
    let subclass = parse_subclass(&raw_sub);
    if subclass.is_none() && !raw_sub.is_empty() {
        let hint = match class {
            Some(c) => format!("for `{}` use one of {}", c.as_str(), subclasses_of(c)),
            None => "see the schema for the subclasses of each class".to_string(),
        };
        out.push(Problem::invalid(
            at,
            "subclass",
            format!("`{raw_sub}` is not a subclass; {hint}"),
        ));
    }
    let (class, subclass) = (class?, subclass?);
    if subclass.class() != class {
        out.push(Problem::invalid(
            at,
            "subclass",
            format!(
                "`{}` belongs to class `{}`, not `{}`; for `{}` use one of {}",
                subclass.as_str(),
                subclass.class().as_str(),
                class.as_str(),
                class.as_str(),
                subclasses_of(class)
            ),
        ));
        return None;
    }
    Some(WorkError {
        class,
        subclass,
        fingerprint: String::new(),
        detail,
        artifact_refs,
        observed_by: OBSERVED_BY.to_string(),
    })
}

fn parse_questions(v: &Value, out: &mut Vec<Problem>) -> Vec<Question> {
    let Some(arr) = v.as_array() else {
        out.push(Problem::invalid(
            "",
            "questions",
            "must be an array of {text, class, options}",
        ));
        return Vec::new();
    };
    if arr.len() > QUESTIONS_MAX {
        out.push(Problem::invalid(
            "",
            "questions",
            format!("has {} entries; ask at most {QUESTIONS_MAX}", arr.len()),
        ));
    }
    let mut questions = Vec::new();
    for (i, q) in arr.iter().enumerate() {
        let at = format!("questions[{i}].");
        let Some(obj) = q.as_object() else {
            out.push(Problem::invalid(
                "",
                &format!("questions[{i}]"),
                "must be an object",
            ));
            continue;
        };
        check_keys(obj, &["text", "class", "options"], &at, out);
        questions.push(Question {
            text: take_text(obj, "text", &at, ENTRY_MAX, true, out),
            class: take_opt_text(obj, "class", &at, NAME_MAX, out).unwrap_or_default(),
            options: take_text_list(obj, "options", &at, OPTIONS_MAX, OPTION_MAX, out),
        });
    }
    questions
}

fn parse_discovered(v: &Value, out: &mut Vec<Problem>) -> Vec<rustykrab_core::work::WorkItemDraft> {
    let Some(arr) = v.as_array() else {
        out.push(Problem::invalid(
            "",
            "discovered",
            "must be an array of drafts",
        ));
        return Vec::new();
    };
    if arr.len() > DISCOVERED_MAX {
        out.push(Problem::invalid(
            "",
            "discovered",
            format!("has {} drafts; keep it to {DISCOVERED_MAX}", arr.len()),
        ));
    }
    // Names first, so a draft may depend on one declared after it.
    let mut tmps = HashSet::new();
    for (i, d) in arr.iter().enumerate() {
        if let Some(t) = d.get("tmp").and_then(Value::as_str).map(str::trim) {
            if !t.is_empty() && !tmps.insert(t.to_string()) {
                out.push(Problem {
                    reason: RejectionReason::DuplicateTmp,
                    detail: format!("discovered[{i}].tmp: `{t}` is used by another draft"),
                });
            }
        }
    }
    arr.iter()
        .enumerate()
        .map(|(i, d)| {
            parse_draft(
                d,
                DraftMode::Discovered { tmps: &tmps },
                &format!("discovered[{i}]."),
                out,
            )
        })
        .collect()
}

/// Parse and check a report. Returns the `item` argument and the report;
/// only meaningful when no problem was added.
fn parse_report(
    args: &Value,
    out: &mut Vec<Problem>,
    summary_max: usize,
) -> (Option<String>, ResultReport) {
    let empty = Map::new();
    let obj = args.as_object().unwrap_or(&empty);
    check_keys(obj, &REPORT_KEYS, "", out);

    let item = take_opt_text(obj, "item", "", POINTER_MAX, out)
        .map(|s| s.trim_start_matches('#').trim().to_string());
    let summary = take_text(obj, "summary", "", summary_max, true, out);
    let artifacts = take_artifacts_of(
        obj,
        "artifacts",
        "",
        ARTIFACTS_MAX,
        &RESULT_ARTIFACT_KINDS,
        out,
    );
    let changed_paths = take_text_list(
        obj,
        "changed_paths",
        "",
        CHANGED_PATHS_MAX,
        POINTER_MAX,
        out,
    );
    let commit = take_opt_text(obj, "commit", "", NAME_MAX, out);
    if let Some(sha) = &commit {
        if !(7..=64).contains(&sha.len()) || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
            out.push(Problem::invalid(
                "",
                "commit",
                "must be a commit SHA (7 to 64 hex digits)",
            ));
        }
    }
    let checks_run = take_text_list(obj, "checks_run", "", CHECKS_MAX, ENTRY_MAX, out);
    let known_limits = take_text_list(obj, "known_limits", "", LIST_MAX, ENTRY_MAX, out);
    let present = |key: &str| obj.get(key).filter(|v| !v.is_null());
    let blocked = present("blocked").and_then(|v| parse_blocked(v, out));
    let error = present("error").and_then(|v| parse_error(v, out));
    let questions = present("questions")
        .map(|v| parse_questions(v, out))
        .unwrap_or_default();
    let discovered = present("discovered")
        .map(|v| parse_discovered(v, out))
        .unwrap_or_default();

    (
        item,
        ResultReport {
            summary,
            artifacts,
            changed_paths,
            commit,
            checks_run,
            known_limits,
            blocked,
            error,
            questions,
            discovered,
        },
    )
}

/// Reports a worker run's typed result through a [`WorkBackend`].
pub struct ResultReportTool {
    backend: Arc<dyn WorkBackend>,
    summary_max: usize,
}

impl ResultReportTool {
    pub fn new(backend: Arc<dyn WorkBackend>) -> Self {
        Self {
            backend,
            summary_max: SUMMARY_MAX,
        }
    }

    /// A bounded complete deliverable for a host that sends the report as content.
    /// Ordinary coding/planning reports retain the compact default.
    pub fn for_full_report(backend: Arc<dyn WorkBackend>) -> Self {
        Self {
            backend,
            summary_max: FULL_REPORT_MAX,
        }
    }
}

/// The tool description, built once around the shared blocked-shape guidance.
static DESCRIPTION: LazyLock<String> = LazyLock::new(|| {
    format!(
        "End your work item with its typed result, as your LAST call. summary: what you did, \
         in a few lines. Pointers, not content: artifacts, changed_paths, commit, checks_run. \
         If you could not finish, set blocked or error (what failed). {} \
         Follow-up work goes in discovered, one draft per item: the controller files it, you \
         do not. The run ends when this call succeeds.",
        *BLOCKED_SHAPE_GUIDANCE
    )
});

static FULL_REPORT_DESCRIPTION: LazyLock<String> = LazyLock::new(|| {
    format!(
        "End your work item with its typed result, as your LAST call. summary is the complete deliverable that the host sends to the configured destination, up to {FULL_REPORT_MAX} characters. Include every verified finding, source link and coverage limit; do not omit required content to fit the ordinary short work-summary format. If you cannot finish, set blocked or error. {} The run ends when this call succeeds.",
        *BLOCKED_SHAPE_GUIDANCE
    )
});

#[async_trait]
impl Tool for ResultReportTool {
    fn name(&self) -> &str {
        "result_report"
    }

    fn description(&self) -> &str {
        if self.summary_max == FULL_REPORT_MAX {
            &FULL_REPORT_DESCRIPTION
        } else {
            &DESCRIPTION
        }
    }

    fn schema(&self) -> ToolSchema {
        let strings =
            |d: &str| json!({ "type": "array", "items": { "type": "string" }, "description": d });
        let subclass_help = CLASSES
            .iter()
            .map(|c| format!("{}: {}", c.as_str(), subclasses_of(*c)))
            .collect::<Vec<_>>()
            .join("; ");
        let summary_description = if self.summary_max == FULL_REPORT_MAX {
            format!("The complete deliverable, including source links and coverage limits (max {} chars). The first line is what later steps see.", self.summary_max)
        } else {
            format!(
                "What you did and found (max {} chars). The first line is what later steps see.",
                self.summary_max
            )
        };
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "item": {
                        "type": "string",
                        "description": "The work item id, only when your brief did not bind one."
                    },
                    "summary": {
                        "type": "string",
                        "description": summary_description
                    },
                    "artifacts": {
                        "type": "array",
                        "description": "Pointers to what you produced.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "kind": { "type": "string", "enum": RESULT_ARTIFACT_KINDS },
                                "value": { "type": "string" }
                            },
                            "required": ["kind", "value"],
                            "additionalProperties": false
                        }
                    },
                    "changed_paths": strings("Files you changed."),
                    "commit": { "type": "string", "description": "Commit SHA, if you committed." },
                    "checks_run": strings("Checks you ran, e.g. `cargo test -p x`."),
                    "known_limits": strings("What is not done or not verified."),
                    "blocked": {
                        "type": "object",
                        "description": "Set when you could not go on.",
                        "properties": {
                            "reason": { "type": "string", "enum": reportable_reasons() },
                            "detail": { "type": "string" },
                            "needs": { "type": "array", "items": { "type": "string" } }
                        },
                        "required": ["reason"],
                        "additionalProperties": false
                    },
                    "error": {
                        "type": "object",
                        "description": format!("Set when the work failed. Subclasses by class: {subclass_help}."),
                        "properties": {
                            "class": { "type": "string", "enum": CLASSES.map(|c| c.as_str()) },
                            "subclass": { "type": "string" },
                            "detail": { "type": "string" },
                            "artifact_refs": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "kind": { "type": "string" },
                                        "value": { "type": "string" }
                                    },
                                    "required": ["kind", "value"],
                                    "additionalProperties": false
                                }
                            }
                        },
                        "required": ["class", "subclass", "detail"],
                        "additionalProperties": false
                    },
                    "questions": {
                        "type": "array",
                        "description": "Questions for the user; the router decides who answers.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "text": { "type": "string" },
                                "class": { "type": "string" },
                                "options": { "type": "array", "items": { "type": "string" } }
                            },
                            "required": ["text"],
                            "additionalProperties": false
                        }
                    },
                    "discovered": {
                        "type": "array",
                        "description": format!("Follow-up work (max {DISCOVERED_MAX}), one draft per item. Each may name at most one supersedes target."),
                        "items": {
                            "type": "object",
                            "properties": draft_properties(true),
                            "required": ["title", "objective", "done_when"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["summary"],
                "additionalProperties": false
            }),
        }
    }

    /// A report is the run's last word: the right next move is to stop.
    fn blocks_turn(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let schema = self.schema();
        validate_tool_args(&schema.parameters, &args).map_err(Error::ToolExecution)?;

        let mut problems = Vec::new();
        let (asked, report) = parse_report(&args, &mut problems, self.summary_max);
        let bound = with_work_run(|r| r.item.clone());
        let item = match (bound, asked) {
            (Some(held), Some(named)) if held != named => {
                problems.push(Problem::invalid(
                    "",
                    "item",
                    format!("you hold item `{held}`; leave item out or pass `{held}`"),
                ));
                None
            }
            (Some(held), _) => Some(held),
            (None, Some(named)) => Some(named),
            (None, None) => {
                problems.push(Problem::invalid(
                    "",
                    "item",
                    "is required: name the work item you are reporting on",
                ));
                None
            }
        };
        let item = match item {
            Some(item) if problems.is_empty() => item,
            _ => {
                let lines: Vec<String> = problems.iter().map(Problem::line).collect();
                return Err(Error::ToolExecution(ToolError::invalid_input(format!(
                    "result_report not accepted; fix every line and call it again:\n- {}",
                    lines.join("\n- ")
                ))));
            }
        };

        let summary = report.summary.clone();
        let discovered = report.discovered.len();
        self.backend
            .report(item.clone(), report, host_provenance())
            .await
            .map_err(|e| {
                Error::ToolExecution(ToolError::internal(format!(
                    "result_report was not recorded: {e}"
                )))
            })?;

        let mut out = json!({
            "ok": true,
            "item": item,
            "summary": summary,
            "note": "reported; the controller verifies it. Stop here.",
        });
        if discovered > 0 {
            out["discovered"] = json!(discovered);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_backend::{StubWorkBackend, WorkCall, WorkRunContext, WORK_RUN_CONTEXT};
    use rustykrab_core::work::{ItemRef, WorkKind};
    use rustykrab_core::ToolErrorKind;

    fn tool(stub: &Arc<StubWorkBackend>) -> ResultReportTool {
        ResultReportTool::new(stub.clone())
    }

    fn run(item: &str) -> WorkRunContext {
        WorkRunContext {
            item: item.into(),
            actor: "worker:pinch".into(),
        }
    }

    fn reports(stub: &StubWorkBackend) -> Vec<(String, ResultReport)> {
        stub.calls()
            .into_iter()
            .filter_map(|c| match c {
                WorkCall::Report { item, report, .. } => Some((item, report)),
                _ => None,
            })
            .collect()
    }

    async fn rejected(stub: &Arc<StubWorkBackend>, args: Value) -> String {
        let err = WORK_RUN_CONTEXT
            .scope(run("item-1"), tool(stub).execute(args))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ToolErrorKind::InvalidInput);
        err.to_string()
    }

    #[tokio::test]
    async fn full_reports_preserve_content_and_reject_overflow_without_recording() {
        let stub = Arc::new(StubWorkBackend::new());
        let full = ResultReportTool::for_full_report(stub.clone());
        let content = "é".repeat(FULL_REPORT_MAX);
        let args = json!({"summary": content});
        assert!(WORK_RUN_CONTEXT
            .scope(run("item-1"), tool(&stub).execute(args.clone()))
            .await
            .is_err());
        assert!(
            reports(&stub).is_empty(),
            "default workers must retain their compact bound"
        );
        let response = WORK_RUN_CONTEXT
            .scope(run("item-1"), full.execute(args))
            .await
            .unwrap();
        assert_eq!(response["summary"], content);
        assert_eq!(reports(&stub)[0].1.summary, content);
        let overflow = WORK_RUN_CONTEXT
            .scope(
                run("item-1"),
                full.execute(json!({"summary":"é".repeat(FULL_REPORT_MAX + 1)})),
            )
            .await
            .unwrap_err();
        assert_eq!(overflow.kind(), ToolErrorKind::InvalidInput);
        assert_eq!(
            reports(&stub).len(),
            1,
            "an over-limit report must not reach the backend"
        );
    }

    #[test]
    fn the_description_shows_where_a_blocked_report_puts_its_question() {
        let tool = tool(&Arc::new(StubWorkBackend::new()));
        let description = tool.description();
        assert!(
            description.contains(
                r#"{"reason": "needs_decision", "detail": "the question or what you need", "needs": []}"#
            ),
            "{description}"
        );
    }

    #[test]
    fn the_guidance_and_the_schema_offer_the_same_blocked_reasons() {
        let schema = tool(&Arc::new(StubWorkBackend::new())).schema();
        let offered: Vec<&str> = schema.parameters["properties"]["blocked"]["properties"]["reason"]
            ["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        // The reasons the guidance names, read back out of its text.
        let listed = BLOCKED_SHAPE_GUIDANCE
            .split_once("its reason one of ")
            .and_then(|(_, rest)| rest.split_once(';'))
            .map(|(list, _)| list)
            .expect("the guidance names its reasons");
        let named: Vec<&str> = listed.split(", ").flat_map(|s| s.split(" or ")).collect();
        assert_eq!(named, offered);
        let fileable: Vec<&str> = BlockedReason::MODEL_FILEABLE
            .iter()
            .map(|r| r.as_str())
            .collect();
        assert_eq!(offered, fileable);
    }

    #[test]
    fn schema_mirrors_the_result_contract() {
        let schema = tool(&Arc::new(StubWorkBackend::new())).schema();
        assert_eq!(schema.name, "result_report");
        let props = schema.parameters["properties"].as_object().unwrap();
        // Every field of `ResultReport`, plus the `item` binding fallback.
        let contract = serde_json::to_value(ResultReport::default()).unwrap();
        let mut want: Vec<&str> = contract
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        want.push("item");
        want.sort();
        let mut got: Vec<&str> = props.keys().map(String::as_str).collect();
        got.sort();
        assert_eq!(got, want);
        assert_eq!(schema.parameters["required"], json!(["summary"]));
        let reasons = props["blocked"]["properties"]["reason"]["enum"].to_string();
        assert!(!reasons.contains("upstream_failed") && !reasons.contains("upstream_expired"));
        assert!(props["discovered"]["items"]["properties"]
            .as_object()
            .unwrap()
            .contains_key("tmp"));
    }

    #[tokio::test]
    async fn a_report_may_land_a_classifier_rule_but_a_draft_may_not_point_at_one() {
        let stub = Arc::new(StubWorkBackend::new());
        let args = json!({
            "summary": "Added a probe for E2E-ZQX-17.",
            "artifacts": [{ "kind": "classifier_rule", "value": "process: e2e-zqx-17" }],
        });
        let out = WORK_RUN_CONTEXT
            .scope(run("item-2"), tool(&stub).execute(args))
            .await
            .unwrap();
        assert_eq!(out["ok"], json!(true));
        let (_, report) = reports(&stub).pop().unwrap();
        assert_eq!(report.artifacts[0].kind, "classifier_rule");

        let drafted = json!({
            "summary": "Found a follow-up.",
            "discovered": [{ "title": "Probe it", "objective": "o", "done_when": "d",
                             "artifact_refs": [{ "kind": "classifier_rule", "value": "process: x" }] }],
        });
        let err = WORK_RUN_CONTEXT
            .scope(run("item-2"), tool(&stub).execute(drafted))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("classifier_rule"), "{err}");
    }

    #[tokio::test]
    async fn a_good_report_reaches_the_backend_and_ends_the_run() {
        let stub = Arc::new(StubWorkBackend::new());
        let args = json!({
            "summary": "Found three plans; B is cheapest at 12 GBP.",
            "artifacts": [{ "kind": "path", "value": "notes/plans.md" }],
            "changed_paths": ["notes/plans.md"],
            "commit": "1d1608b",
            "checks_run": ["compared list prices"],
            "known_limits": ["prices exclude roaming"],
            "questions": [{ "text": "Switch now?", "class": "blocking_later", "options": ["yes", "no"] }],
            "discovered": [
                { "tmp": "a", "title": "Port the number", "objective": "Move the number to B",
                  "done_when": "Number works on B", "kind": "personal" },
                { "title": "Cancel old plan", "objective": "End plan A",
                  "done_when": "Plan A closed", "edges": [{ "kind": "blocks", "depends_on": "a" }] }
            ]
        });
        let out = WORK_RUN_CONTEXT
            .scope(run("item-1"), tool(&stub).execute(args))
            .await
            .unwrap();
        assert_eq!(out["ok"], json!(true));
        assert_eq!(out["item"], "item-1");
        assert_eq!(out["discovered"], json!(2));
        assert_eq!(
            run_end_summary("result_report", &out).as_deref(),
            Some("Found three plans; B is cheapest at 12 GBP.")
        );

        let calls = stub.calls();
        assert_eq!(
            calls.len(),
            1,
            "drafts are never filed from here: {calls:?}"
        );
        let (item, report) = reports(&stub).pop().unwrap();
        assert_eq!(item, "item-1");
        assert_eq!(report.commit.as_deref(), Some("1d1608b"));
        assert_eq!(report.questions[0].options, vec!["yes", "no"]);
        assert_eq!(report.discovered.len(), 2);
        assert_eq!(report.discovered[0].kind, Some(WorkKind::Personal));
        assert_eq!(
            report.discovered[1].edges[0].depends_on,
            ItemRef::Tmp { tmp: "a".into() },
            "a sibling's tmp name resolves to a temp ref"
        );
        if let WorkCall::Report { provenance, .. } = &calls[0] {
            assert_eq!(provenance.filed_by_item.as_deref(), Some("item-1"));
            assert_eq!(provenance.actor, "worker:pinch");
        }
    }

    #[tokio::test]
    async fn the_item_binds_from_the_run_else_from_the_argument() {
        let stub = Arc::new(StubWorkBackend::new());
        let t = tool(&stub);

        let err = t.execute(json!({ "summary": "done" })).await.unwrap_err();
        assert!(err.to_string().contains("item: is required"), "{err}");

        t.execute(json!({ "summary": "done", "item": "#item-5" }))
            .await
            .unwrap();
        assert_eq!(reports(&stub).pop().unwrap().0, "item-5");

        let msg = rejected(&stub, json!({ "summary": "done", "item": "item-2" })).await;
        assert!(msg.contains("you hold item `item-1`"), "{msg}");
        assert_eq!(reports(&stub).len(), 1);
    }

    #[tokio::test]
    async fn a_cascade_reason_cannot_be_reported() {
        let stub = Arc::new(StubWorkBackend::new());
        for reason in ["upstream_failed", "upstream_expired"] {
            let msg = rejected(
                &stub,
                json!({ "summary": "stuck", "blocked": { "reason": reason } }),
            )
            .await;
            assert!(
                msg.contains("set only by the controller's cascade"),
                "{msg}"
            );
        }
        let msg = rejected(
            &stub,
            json!({ "summary": "stuck", "blocked": { "reason": "bored" } }),
        )
        .await;
        assert!(
            msg.contains("`bored` is not one of needs_tool, needs_credential"),
            "{msg}"
        );
        let msg = rejected(
            &stub,
            json!({ "summary": "stuck", "blocked": { "reason": "budget_exhausted" } }),
        )
        .await;
        assert!(msg.contains("set only by the controller;"), "{msg}");
        assert!(stub.calls().is_empty());

        WORK_RUN_CONTEXT
            .scope(
                run("item-1"),
                tool(&stub).execute(json!({
                    "summary": "need a login",
                    "blocked": { "reason": "needs_credential", "detail": "bank login", "needs": ["bank_password"] }
                })),
            )
            .await
            .unwrap();
        let report = reports(&stub).pop().unwrap().1;
        assert_eq!(
            report.blocked.unwrap().reason,
            BlockedReason::NeedsCredential
        );
    }

    #[tokio::test]
    async fn error_class_and_subclass_must_agree() {
        let stub = Arc::new(StubWorkBackend::new());
        let msg = rejected(
            &stub,
            json!({ "summary": "failed", "error": { "class": "model", "subclass": "timeout", "detail": "slow" } }),
        )
        .await;
        assert!(
            msg.contains("`timeout` belongs to class `tool`, not `model`"),
            "{msg}"
        );
        assert!(stub.calls().is_empty());

        // The plan's word for a missing tool is `tool`, under capability_gap.
        for sub in ["tool"] {
            WORK_RUN_CONTEXT
                .scope(
                    run("item-1"),
                    tool(&stub).execute(json!({
                        "summary": "no tool",
                        "error": { "class": "capability_gap", "subclass": sub, "detail": "no OCR tool" }
                    })),
                )
                .await
                .unwrap();
            let err = reports(&stub).pop().unwrap().1.error.unwrap();
            assert_eq!(err.subclass, ErrorSubclass::ToolGap);
            assert_eq!(err.class, ErrorClass::CapabilityGap);
            assert!(err.fingerprint.is_empty(), "the controller fingerprints");
            assert_eq!(err.observed_by, OBSERVED_BY);
        }

        let msg = rejected(
            &stub,
            json!({ "summary": "x", "error": { "class": "tool", "subclass": "timeout", "detail": "d", "fingerprint": "abc" } }),
        )
        .await;
        assert!(msg.contains("the controller classifies errors"), "{msg}");
    }

    #[tokio::test]
    async fn a_draft_may_name_at_most_one_supersedes_target() {
        let stub = Arc::new(StubWorkBackend::new());
        let msg = rejected(
            &stub,
            json!({
                "summary": "found more",
                "discovered": [{
                    "title": "Retry", "objective": "o", "done_when": "d",
                    "supersedes": "old-1",
                    "edges": [{ "kind": "supersedes", "depends_on": "old-2" }]
                }]
            }),
        )
        .await;
        assert!(
            msg.contains(
                "discovered[0].supersedes: a draft may name at most one supersedes target"
            ),
            "{msg}"
        );
        let msg = rejected(
            &stub,
            json!({
                "summary": "found more",
                "discovered": [{ "title": "t", "objective": "o", "done_when": "d",
                                 "supersedes": ["old-1", "old-2"] }]
            }),
        )
        .await;
        assert!(msg.contains("at most one supersedes target"), "{msg}");
        assert!(stub.calls().is_empty());
    }

    #[tokio::test]
    async fn every_problem_comes_back_at_once() {
        let stub = Arc::new(StubWorkBackend::new());
        let msg = rejected(
            &stub,
            json!({
                "summary": "  ",
                "commit": "not-a-sha",
                "status": "done",
                "discovered": [
                    { "tmp": "a", "title": "t", "objective": "o", "done_when": "d", "kind": "code" },
                    { "tmp": "a", "title": "", "objective": "o", "done_when": "d",
                      "edges": [{ "kind": "blocks", "depends_on": { "tmp": "zzz" } }] }
                ]
            }),
        )
        .await;
        for needle in [
            "invalid_item: summary: is empty",
            "commit: must be a commit SHA",
            "status: not yours to set",
            "kind_not_allowed: discovered[0].kind",
            "duplicate_tmp: discovered[1].tmp",
            "discovered[1].title: is empty",
            "unknown_ref: discovered[1].edges[0].depends_on",
        ] {
            assert!(msg.contains(needle), "missing {needle:?} in {msg}");
        }
    }

    #[tokio::test]
    async fn schema_validation_catches_malformed_args() {
        let stub = Arc::new(StubWorkBackend::new());
        let err = tool(&stub).execute(json!({})).await.unwrap_err();
        assert!(err.to_string().contains("'summary'"), "{err}");
        let err = tool(&stub)
            .execute(json!({ "summary": "s", "changed_paths": "a.rs" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("changed_paths"), "{err}");
        assert!(stub.calls().is_empty());
    }

    #[tokio::test]
    async fn a_backend_failure_does_not_end_the_run() {
        let stub = Arc::new(StubWorkBackend::new());
        stub.fail_reports("lease expired");
        let err = WORK_RUN_CONTEXT
            .scope(
                run("item-1"),
                tool(&stub).execute(json!({ "summary": "done" })),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("lease expired"), "{err}");
    }

    #[test]
    fn the_run_ends_only_on_a_successful_run_ending_call() {
        let tool = ResultReportTool::new(Arc::new(StubWorkBackend::new()));
        assert!(tool.blocks_turn());
        // `task_complete`'s success shape reads the same way.
        let tc = json!({ "ok": true, "summary": "found 3 hotels" });
        assert_eq!(
            run_end_summary("task_complete", &tc).as_deref(),
            Some("found 3 hotels")
        );
        assert_eq!(run_end_summary("work_file", &tc), None);
        assert_eq!(
            run_end_summary("result_report", &json!({ "ok": false, "summary": "x" })),
            None
        );
        assert_eq!(
            run_end_summary("result_report", &json!({ "ok": true, "summary": "  " })),
            None
        );
    }

    #[test]
    fn every_subclass_is_listed_once_and_parses_by_its_word() {
        let words: HashSet<&str> = SUBCLASSES.iter().map(|s| s.as_str()).collect();
        assert_eq!(words.len(), SUBCLASSES.len());
        for s in SUBCLASSES {
            assert_eq!(parse_subclass(s.as_str()), Some(s));
            assert!(CLASSES.contains(&s.class()));
        }
    }
}
