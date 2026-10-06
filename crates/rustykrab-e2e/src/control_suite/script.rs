//! The control suite's scripted agent: the orchestration triggers its
//! scenarios send and the worker behaviours an item's objective selects.
//! `main.rs` merges [`agent_script_scenarios`] into the daemon's script.

use serde_json::{json, Value};

use crate::fixture_repo::TIDE_TABLE_BUILT;

// ── the scripted agent ───────────────────────────────────────────────
//
// Orchestration triggers are user messages a scenario sends. Worker
// triggers sit in an item's objective and select what its worker does.
// Matching is case-insensitive and the longest matching trigger wins, which
// two pairs rely on: a resumed brief carries both the original objective
// and the line that should now win ([`ANSWER_FLORIST`] over [`W_ASK`],
// [`TIDE_TABLE_BUILT`] over [`W_TIDE`]).

pub(super) const S01_FILE: &str = "e2e-control s01: file the dentist booking";
pub(super) const S01_TITLE: &str = "Book the dentist [e2e-control s01]";
pub(super) const S01_WORKER: &str = "e2e-control worker s01: book the dentist";
pub(super) const S01_EVIDENCE: &str = "e2e-control-s01-confirmation";
pub(super) const S11_UNLOADED: &str = "e2e-control s11: file with an unloaded tool";
pub(super) const S11_UNLOADED_TITLE: &str = "Check the calendar [e2e-control s11 unloaded]";
pub(super) const S11_MCP: &str = "e2e-control s11: file with an unconfigured mcp server";
pub(super) const S11_MCP_TITLE: &str = "Read the e2e board [e2e-control s11 mcp]";
pub(super) const S18_REQUEST: &str = "e2e-control s18: plan the lisbon trip";
pub(super) const S18_TITLE: &str = "Plan the Lisbon trip [e2e-control s18]";
pub(super) const S18_PLANNER: &str = "e2e-control planner s18: plan the lisbon trip";
pub(super) const S18_DRAFT_ONLY: &str = "Pick a restaurant [e2e-control s18 rejected draft]";
pub(super) const S30_TURN: &str = "e2e-control s30: interactive turn";

pub(super) const W_SUCCEED: &str = "e2e-control worker: succeed";
pub(super) const W_PROBE: &str = "e2e-control worker: report the active tools";
pub(super) const W_HOLD: &str = "e2e-control worker: hold the resource for three seconds";
pub(super) const W_SLOW: &str = "e2e-control worker: work for twenty seconds";
/// A peer's run long enough to restart either daemon inside it (8).
pub(super) const W_PEER_PAUSE: &str = "e2e-control worker: work for eight seconds on the peer";
pub(super) const W_TIMEOUT: &str = "e2e-control worker: fail with a timeout";
pub(super) const W_UNRECOGNISED: &str =
    "e2e-control worker: fail in a way no classifier recognises";
/// The raw failure [`W_UNRECOGNISED`] reports, and the trigger of the
/// internal item that adds its probe (shorter, so a repair brief that
/// carries both still replays the failure).
pub(super) const RAW_FAILURE: &str = "E2E-ZQX-17 flux capacitor desynchronised";
pub(super) const W_PROBE_FIX: &str = "e2e-zqx-17";
/// The classifier rule the internal item of 14 lands: `<subclass>:
/// <pattern>` (section 9's extension point, as data).
pub(super) const PROBE_RULE: &str = "process: e2e-zqx-17";
pub(super) const W_DOC_ALPHA: &str = "e2e-control worker: attach document alpha";
pub(super) const W_DOC_BETA_SLOW: &str = "e2e-control worker: attach document beta after a pause";
pub(super) const W_DOC_GAMMA: &str = "e2e-control worker: attach document gamma";
pub(super) const W_READ_INPUTS: &str = "e2e-control worker: read the inputs slowly";
pub(super) const W_TIDE: &str = "e2e-control worker: read the tide table";
pub(super) const W_ASK: &str = "e2e-control worker: ask which florist";
pub(super) const FLORIST_QUESTION: &str = "Which florist should e2e-control use, Petals or Stems?";
pub(super) const ANSWER_FLORIST: &str = "e2e-control answer: use petals, the one on the corner";
pub(super) const W_ASK_DEFAULT: &str = "e2e-control worker: ask about the reminder time";
pub(super) const DEFAULTABLE_QUESTION: &str = "Should the e2e-control reminder use the usual 9am?";
pub(super) const W_ASK_SURFACED: &str = "e2e-control worker: ask when to check in";
pub(super) const W_STALL: &str = "e2e-control worker: make no progress";
pub(super) const W_CODE_LOCAL: &str = "e2e-control worker: edit the fixture locally";
pub(super) const W_CODE_BAD: &str = "e2e-control worker: claim an edit that never happened";

/// One scripted assistant turn that calls one tool.
pub(super) fn call(name: &str, arguments: Value) -> Value {
    json!({ "toolCalls": [ { "name": name, "arguments": arguments } ] })
}

pub(super) fn done(summary: &str) -> Value {
    call("task_complete", json!({ "summary": summary }))
}

pub(super) fn report(arguments: Value) -> Value {
    call("result_report", arguments)
}

pub(super) fn succeeded(summary: &str, artifact: (&str, &str)) -> Value {
    report(json!({
        "summary": summary,
        "artifacts": [ { "kind": artifact.0, "value": artifact.1 } ],
    }))
}

/// Loads `names` first, as a model must before it calls a tool its run
/// did not declare (plan section 12): the host runs only a tool that is
/// declared or appended.
pub(super) fn load(names: &[&str]) -> Value {
    call("tools_load", json!({ "names": names }))
}

/// Reads the conversation's active tool set without changing what matters:
/// `tools_load` answers with every active name, and `todo_read` is active
/// from turn 0 anyway.
pub(super) fn probe_active_tools() -> Value {
    call("tools_load", json!({ "names": ["todo_read"] }))
}

pub(super) fn script(trigger: &str, steps: Vec<Value>) -> Value {
    json!({ "trigger": trigger, "steps": steps })
}

pub(super) fn budget(tokens: u64) -> Value {
    json!({ "iterations": 10, "tokens": tokens, "wall_seconds": 600, "repairs": 2 })
}

/// A parent's budget: `tokens`, and room for ten [`budget`]s' iterations
/// and wall time, since a parent's budget is an envelope over its
/// children's in every dimension (plan sections 4.2 and 14.1).
pub(super) fn envelope(tokens: u64) -> Value {
    json!({ "iterations": 100, "tokens": tokens, "wall_seconds": 6000, "repairs": 2 })
}

/// The control suite's part of the scripted daemon's script, merged into
/// `AGENT_SCRIPT` by `main.rs`.
pub(crate) fn agent_script_scenarios() -> Vec<Value> {
    let evidence = ("message", "e2e-control-evidence");
    let s18_item = |tmp: &str, parent: Option<&str>, title: &str, worker: &str| {
        json!({
            "tmp": tmp,
            "parent": parent.map(|p| json!({ "tmp": p })),
            "kind": "personal",
            "title": title,
            "objective": format!("{worker}. {title}"),
            "done_when": format!("{title} is confirmed"),
            "budget": if parent.is_none() { envelope(200_000) } else { budget(20_000) },
        })
    };
    let s18_edge = |item: &str, depends_on: &str| json!({ "item": { "tmp": item }, "kind": "blocks", "depends_on": { "tmp": depends_on } });
    vec![
        // 1: the orchestration conversation loads what the task needs, and
        // `work_file`, then files it; its worker reports the active set
        // from its first step.
        script(
            S01_FILE,
            vec![
                load(&["caldav", "work_file"]),
                call(
                    "work_file",
                    json!({
                        "kind": "personal",
                        "title": S01_TITLE,
                        "objective": format!("{S01_WORKER} for next Tuesday morning"),
                        "done_when": "a confirmed appointment is attached as evidence",
                        "required_tools": ["caldav"],
                        "writable_resources": ["calendar"],
                    }),
                ),
                done("Queued the dentist booking."),
            ],
        ),
        script(
            S01_WORKER,
            vec![
                probe_active_tools(),
                succeeded(
                    "Booked the dentist for Tuesday 09:30.",
                    ("message", S01_EVIDENCE),
                ),
                done("Booked the dentist."),
            ],
        ),
        // 11: a registered tool this conversation never loaded, then an MCP
        // server nobody configured.
        script(
            S11_UNLOADED,
            vec![
                load(&["work_file"]),
                call(
                    "work_file",
                    json!({
                        "kind": "personal",
                        "title": S11_UNLOADED_TITLE,
                        "objective": format!("{W_SUCCEED}. Check Saturday on the calendar"),
                        "done_when": "Saturday's events are listed",
                        "required_tools": ["caldav"],
                    }),
                ),
                done("Tried to file the calendar check."),
            ],
        ),
        script(
            S11_MCP,
            vec![
                load(&["work_file"]),
                call(
                    "work_file",
                    json!({
                        "kind": "personal",
                        "title": S11_MCP_TITLE,
                        "objective": format!("{W_SUCCEED}. Read the board"),
                        "done_when": "the board's open cards are listed",
                        "required_mcp_servers": ["e2e-control-unconfigured"],
                    }),
                ),
                done("Filed the board read."),
            ],
        ),
        // 18: the request asks for a plan; the planner's first graph puts
        // an ordering edge between an item and its own child, its second is
        // sound. The root is a temp id: a static script cannot name the
        // request's real id.
        script(
            S18_REQUEST,
            vec![
                load(&["work_file"]),
                call(
                    "work_file",
                    json!({
                        "kind": "personal",
                        "title": S18_TITLE,
                        "objective": S18_PLANNER,
                        "done_when": "flights and hotel booked and in the calendar",
                        "plan": true,
                    }),
                ),
                done("Planning the Lisbon trip."),
            ],
        ),
        script(
            S18_PLANNER,
            vec![
                call(
                    "work_plan",
                    json!({
                        "root": { "tmp": "trip" },
                        "items": [
                            s18_item("trip", None, "Lisbon trip, 3 to 6 May [e2e-control s18]", "Lisbon trip"),
                            s18_item("flights", Some("trip"), "Book the flights [e2e-control s18]", W_SUCCEED),
                            s18_item("hotel", Some("flights"), "Book the hotel [e2e-control s18]", W_SUCCEED),
                            s18_item("restaurant", Some("trip"), S18_DRAFT_ONLY, W_SUCCEED),
                        ],
                        "edges": [ s18_edge("flights", "hotel") ],
                        "rationale": "flights wait on the hotel",
                    }),
                ),
                call(
                    "work_plan",
                    json!({
                        "root": { "tmp": "trip" },
                        "items": [
                            s18_item("trip", None, "Lisbon trip, 3 to 6 May [e2e-control s18]", "Lisbon trip"),
                            s18_item("flights", Some("trip"), "Book the flights [e2e-control s18]", W_SUCCEED),
                            s18_item("hotel", Some("trip"), "Book the hotel [e2e-control s18]", W_SUCCEED),
                            s18_item("calendar", Some("trip"), "Add the trip to the calendar [e2e-control s18]", W_SUCCEED),
                        ],
                        "edges": [ s18_edge("calendar", "flights"), s18_edge("calendar", "hotel") ],
                        "rationale": "book both, then put the trip in the calendar",
                    }),
                ),
                done("Planned the Lisbon trip."),
            ],
        ),
        // 30: an interactive turn long enough for a due job to fall inside it.
        script(
            S30_TURN,
            vec![
                load(&["exec"]),
                call("exec", json!({ "command": "sleep 6", "timeout_secs": 20 })),
                done("Finished the interactive turn."),
            ],
        ),
        // Worker behaviours, selected by an item's objective.
        script(
            W_SUCCEED,
            vec![succeeded("Finished.", evidence), done("Finished.")],
        ),
        script(
            W_PROBE,
            vec![
                probe_active_tools(),
                succeeded("Reported the active tools.", evidence),
                done("Reported."),
            ],
        ),
        script(
            W_HOLD,
            vec![
                load(&["exec"]),
                call("exec", json!({ "command": "sleep 3", "timeout_secs": 20 })),
                succeeded("Held the resource.", evidence),
                done("Held the resource."),
            ],
        ),
        script(
            W_SLOW,
            vec![
                load(&["exec"]),
                call("exec", json!({ "command": "sleep 20", "timeout_secs": 25 })),
                succeeded("Worked for twenty seconds.", evidence),
                done("Worked for twenty seconds."),
            ],
        ),
        script(
            W_PEER_PAUSE,
            vec![
                load(&["exec"]),
                call("exec", json!({ "command": "sleep 8", "timeout_secs": 20 })),
                succeeded("Worked for eight seconds on the peer.", evidence),
                done("Worked on the peer."),
            ],
        ),
        script(
            W_TIMEOUT,
            vec![
                report(json!({
                    "summary": "The booking service timed out.",
                    "error": {
                        "class": "tool", "subclass": "timeout",
                        "detail": "upstream timed out after 30s",
                    },
                })),
                done("Timed out."),
            ],
        ),
        script(
            W_UNRECOGNISED,
            vec![
                report(json!({
                    "summary": "Stopped on an error I do not recognise.",
                    "error": {
                        "class": "unknown", "subclass": "unclassified",
                        "detail": RAW_FAILURE,
                    },
                })),
                done("Stopped."),
            ],
        ),
        // 14: the internal item lands its probe as a classifier rule the
        // controller replays against the failure before it counts.
        script(
            W_PROBE_FIX,
            vec![
                report(json!({
                    "summary": "Added a probe that classifies E2E-ZQX-17.",
                    "artifacts": [
                        { "kind": "path", "value": "e2e-control/probe.rs" },
                        { "kind": "classifier_rule", "value": PROBE_RULE },
                    ],
                })),
                done("Added the probe."),
            ],
        ),
        script(
            W_DOC_ALPHA,
            vec![
                succeeded("Attached document alpha.", ("path", "e2e-control/alpha.md")),
                done("Attached alpha."),
            ],
        ),
        script(
            W_DOC_BETA_SLOW,
            vec![
                load(&["exec"]),
                call("exec", json!({ "command": "sleep 4", "timeout_secs": 20 })),
                succeeded("Attached document beta.", ("path", "e2e-control/beta.md")),
                done("Attached beta."),
            ],
        ),
        script(
            W_DOC_GAMMA,
            vec![
                succeeded("Attached document gamma.", ("path", "e2e-control/gamma.md")),
                done("Attached gamma."),
            ],
        ),
        script(
            W_READ_INPUTS,
            vec![
                load(&["exec"]),
                call("exec", json!({ "command": "sleep 3", "timeout_secs": 20 })),
                succeeded("Chose from the inputs.", evidence),
                done("Chose."),
            ],
        ),
        // 13: the tool does not exist, so the worker reports needs_tool; once
        // the capability item is done, the resumed brief carries its summary.
        script(
            W_TIDE,
            vec![
                call("tide_table", json!({ "port": "Lisbon" })),
                report(json!({
                    "summary": "No tool reads tide tables.",
                    "blocked": { "reason": "needs_tool", "detail": "no tool reads tide tables", "needs": ["tide_table"] },
                })),
                done("Blocked on a missing tool."),
            ],
        ),
        script(
            TIDE_TABLE_BUILT,
            vec![
                probe_active_tools(),
                call("tide_table", json!({ "port": "Lisbon" })),
                succeeded("High water at 14:05.", evidence),
                done("Read the tide table."),
            ],
        ),
        // 3 and 15: questions, one the router must keep from the user.
        script(
            W_ASK,
            vec![
                report(json!({
                    "summary": "I need a decision.",
                    "blocked": { "reason": "needs_decision", "detail": "which florist" },
                    "questions": [ { "text": FLORIST_QUESTION, "class": "blocking_now", "options": ["Petals", "Stems"] } ],
                })),
                done("Asked which florist."),
            ],
        ),
        script(
            ANSWER_FLORIST,
            vec![
                succeeded("Ordered the flowers from Petals.", evidence),
                done("Ordered."),
            ],
        ),
        script(
            W_ASK_DEFAULT,
            vec![
                report(json!({
                    "summary": "I need the reminder time.",
                    "blocked": { "reason": "needs_decision", "detail": "reminder time" },
                    "questions": [ { "text": DEFAULTABLE_QUESTION, "class": "defaultable", "options": ["9am"] } ],
                })),
                done("Asked about the reminder time."),
            ],
        ),
        script(
            W_ASK_SURFACED,
            vec![
                report(json!({
                    "summary": "I need the check-in time.",
                    "blocked": { "reason": "needs_decision", "detail": "check-in time" },
                    "questions": [ {
                        "text": "What time should the e2e-control check-in run? The recorded default is 9am.",
                        "class": "blocking_now", "options": ["9am", "10am"],
                    } ],
                })),
                done("Asked when to check in."),
            ],
        ),
        // 5: turns that add no evidence, for the progress ledger to catch.
        script(
            W_STALL,
            std::iter::repeat_n(call("todo_read", json!({})), 12)
                .chain([done("Still looking.")])
                .collect(),
        ),
        // 17: a local worker that commits in its worktree, and one that
        // claims a commit that does not exist.
        script(
            W_CODE_LOCAL,
            vec![
                load(&["exec"]),
                call(
                    "exec",
                    json!({ "command": "echo '// e2e-control local change' >> src/lib.rs && git add src/lib.rs && git -c user.name=e2e -c user.email=e2e@rustykrab.invalid commit -q --no-gpg-sign -m e2e-control-local-change" }),
                ),
                report(
                    json!({ "summary": "Documented the helper.", "changed_paths": ["src/lib.rs"] }),
                ),
                done("Committed."),
            ],
        ),
        script(
            W_CODE_BAD,
            vec![
                report(json!({
                    "summary": "Documented the helper.",
                    "changed_paths": ["src/lib.rs"],
                    "commit": "0000000000000000000000000000000000000000",
                })),
                done("Claimed a commit."),
            ],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustykrab_core::work::{ResultReport, WorkItemDraft, WorkPlan};

    #[test]
    fn every_named_behaviour_has_a_script_and_no_trigger_contains_another() {
        let triggers: Vec<String> = agent_script_scenarios()
            .iter()
            .map(|s| s["trigger"].as_str().unwrap().to_lowercase())
            .collect();
        for named in [
            S01_FILE,
            S01_WORKER,
            S11_UNLOADED,
            S11_MCP,
            S18_REQUEST,
            S18_PLANNER,
            S30_TURN,
            W_SUCCEED,
            W_PROBE,
            W_HOLD,
            W_SLOW,
            W_PEER_PAUSE,
            W_TIMEOUT,
            W_UNRECOGNISED,
            W_PROBE_FIX,
            W_DOC_ALPHA,
            W_DOC_BETA_SLOW,
            W_DOC_GAMMA,
            W_READ_INPUTS,
            W_TIDE,
            TIDE_TABLE_BUILT,
            W_ASK,
            ANSWER_FLORIST,
            W_ASK_DEFAULT,
            W_ASK_SURFACED,
            W_STALL,
            W_CODE_LOCAL,
            W_CODE_BAD,
        ] {
            assert!(
                triggers.contains(&named.to_lowercase()),
                "{named} has no script"
            );
        }
        for (i, a) in triggers.iter().enumerate() {
            for b in &triggers[i + 1..] {
                assert!(
                    !a.contains(b.as_str()) && !b.contains(a.as_str()),
                    "{a:?} and {b:?}"
                );
            }
        }
    }

    /// A resumed brief carries the original objective and the line that
    /// should now win; the scripted provider replays the longest match.
    #[test]
    fn resume_lines_outrank_the_objectives_they_resume() {
        assert!(ANSWER_FLORIST.len() > W_ASK.len());
        assert!(TIDE_TABLE_BUILT.len() > W_TIDE.len());
        assert!(W_UNRECOGNISED.len() > W_PROBE_FIX.len());
        assert!(RAW_FAILURE.to_lowercase().contains(W_PROBE_FIX));
    }

    /// The scripted calls must parse as the shared vocabulary, or a
    /// scenario would fail on its own typo long after the phase shipped.
    #[test]
    fn scripted_calls_speak_the_shared_vocabulary() {
        for scenario in agent_script_scenarios() {
            for step in scenario["steps"].as_array().unwrap() {
                for call in step["toolCalls"].as_array().into_iter().flatten() {
                    let mut args = call["arguments"].clone();
                    match call["name"].as_str().unwrap() {
                        "work_file" => {
                            serde_json::from_value::<WorkItemDraft>(args).unwrap();
                        }
                        "work_plan" => {
                            serde_json::from_value::<WorkPlan>(args).unwrap();
                        }
                        "result_report" => {
                            // A worker reports class, subclass and detail;
                            // the fingerprint and observer are the
                            // controller's, and the tool refuses them.
                            if let Some(error) = args["error"].as_object() {
                                for key in error.keys() {
                                    assert!(
                                        ["class", "subclass", "detail", "artifact_refs"]
                                            .contains(&key.as_str()),
                                        "result_report error.{key} is not the worker's to set"
                                    );
                                }
                                // What the tool fills in before the report
                                // reaches the controller.
                                args["error"]["fingerprint"] = json!("");
                                args["error"]["observed_by"] = json!("worker_report");
                            }
                            serde_json::from_value::<ResultReport>(args).unwrap();
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}
