//! Plan scenario 10 (`docs/plans/control-layer-and-worker-fleet.md`,
//! section 15; Phase 2's exit): the distractor-catalog matrix of the late
//! binding experiment (`research_notes/Late tool binding experiment/`,
//! `late_binding_catalog.py`), run through RustyKrab's own append path
//! instead of a hand-built request.
//!
//! Each case boots the daemon with the experiment's 13-tool base declared
//! from turn 0 (the meta and seed tools, kept real) and a catalog of stub
//! tools that are registered but not declared (`"visible": false`): the
//! task's target buried among four or nine near-misses, or the near-misses
//! alone. The model is asked to search with `tools_list`; the search
//! appends what the host finds plausible as text, and the model calls it.
//!
//! A case passes when the target is called with the task's key argument and
//! every request of the run declared the same tools array (the provider's
//! `tool block sent` log). The target never enters the array, so a pass
//! means it was called by append. A missing case passes when no near-miss
//! is called in the target's place: the host told the model nothing
//! matched.
//!
//! The experiment's 12 of 12 is three tasks at four repetitions: run
//! `--mode model --case late-binding --reps 4`, once with `--model
//! gemma4:26b` and once with `--model qwen3.8:27b-mlx`.

use serde_json::{json, Value};

use crate::assertion::Assertion;
use crate::model_suite::{bounded_harness, ModelCase};

/// The late binding experiment's window (2026-09-24). Scenario 10 measures
/// whether a tool found among near-misses is called by append, not how a
/// tight window compacts: at the model suite's 6,144 the base tools'
/// schemas and the reply reserve leave no room for the turn, and every
/// request is refused before it reaches the model.
const LATE_BINDING_NUM_CTX: u32 = 32_768;

/// The experiment's base: RustyKrab's meta and seed tools, with their real
/// descriptions. Kept real rather than stubbed, since the append path is
/// theirs.
const BASE: &[&str] = &[
    "tools_list",
    "tools_load",
    "recall_append",
    "recall_info",
    "recall_peek",
    "recall_search",
    "recall_sub_query",
    "skills",
    "memory_search",
    "memory_save",
    "todo_write",
    "todo_read",
    "credential_request",
];

struct Task {
    name: &'static str,
    user: &'static str,
    target: fn() -> Value,
    /// The argument that shows the call was for this request.
    pointer: &'static str,
    needle: &'static str,
    distractors: fn() -> Vec<Value>,
}

/// A catalog stub: registered, not declared from turn 0.
fn stub(name: &str, description: &str, properties: Value, answer: Value) -> Value {
    let required: Vec<String> = properties
        .as_object()
        .and_then(|p| p.keys().next().cloned())
        .into_iter()
        .collect();
    json!({
        "name": name,
        "description": description,
        "parameters": { "type": "object", "properties": properties, "required": required },
        "script": { "responses": [ { "type": "ok", "value": answer } ] },
        "visible": false,
    })
}

/// A near-miss: plausible-looking, answers like a real tool would.
fn near(name: &str, description: &str, params: &[(&str, &str)]) -> Value {
    let properties: serde_json::Map<String, Value> = params
        .iter()
        .map(|(p, t)| ((*p).to_string(), json!({ "type": t })))
        .collect();
    stub(
        name,
        description,
        Value::Object(properties),
        json!({ "ok": true, "tool": name }),
    )
}

fn tasks() -> [Task; 3] {
    [
        Task {
            name: "weather",
            user: "What's the weather in Lisbon right now? Use celsius.",
            target: || {
                stub(
                    "get_weather",
                    "Get the current weather for a city.",
                    json!({
                        "city": { "type": "string" },
                        "unit": { "type": "string", "enum": ["celsius", "fahrenheit"] },
                    }),
                    json!({ "city": "Lisbon", "temperature_c": 19, "conditions": "light rain" }),
                )
            },
            pointer: "/city",
            needle: "lisbon",
            distractors: || {
                vec![
                    near(
                        "get_forecast",
                        "Get a multi-day weather forecast for a city.",
                        &[("city", "string"), ("days", "integer")],
                    ),
                    near(
                        "get_weather_history",
                        "Get recorded weather for a city on a past date.",
                        &[("city", "string"), ("date", "string")],
                    ),
                    near(
                        "get_air_quality",
                        "Get the current air quality index for a city.",
                        &[("city", "string")],
                    ),
                    near(
                        "get_sunrise_sunset",
                        "Get today's sunrise and sunset times for a city.",
                        &[("city", "string")],
                    ),
                    near(
                        "convert_temperature",
                        "Convert a temperature between celsius and fahrenheit.",
                        &[("value", "number"), ("to_unit", "string")],
                    ),
                    near(
                        "get_timezone",
                        "Get the IANA timezone and current local time for a city.",
                        &[("city", "string")],
                    ),
                    near(
                        "get_pollen_index",
                        "Get today's pollen index for a city.",
                        &[("city", "string")],
                    ),
                    near(
                        "get_marine_conditions",
                        "Get sea state and swell for a coastal location.",
                        &[("location", "string")],
                    ),
                    near(
                        "get_uv_index",
                        "Get the current UV index for a city.",
                        &[("city", "string")],
                    ),
                ]
            },
        },
        Task {
            name: "calendar",
            user: "Put a 30-minute dentist appointment on my calendar for 2026-10-02 at 09:30.",
            target: || {
                stub(
                    "create_calendar_event",
                    "Create an event on the user's calendar.",
                    json!({
                        "title": { "type": "string" },
                        "start": { "type": "string", "description": "ISO 8601 start time" },
                        "duration_minutes": { "type": "integer" },
                    }),
                    json!({ "created": true, "event_id": "evt-5521" }),
                )
            },
            pointer: "/title",
            needle: "dentist",
            distractors: || {
                vec![
                    near(
                        "list_calendar_events",
                        "List events on the user's calendar in a date range.",
                        &[("start", "string"), ("end", "string")],
                    ),
                    near(
                        "create_reminder",
                        "Create a reminder notification at a given time.",
                        &[("text", "string"), ("at", "string")],
                    ),
                    near(
                        "update_calendar_event",
                        "Change the time or title of an existing calendar event.",
                        &[
                            ("event_id", "string"),
                            ("title", "string"),
                            ("start", "string"),
                        ],
                    ),
                    near(
                        "delete_calendar_event",
                        "Delete a calendar event by id.",
                        &[("event_id", "string")],
                    ),
                    near(
                        "find_free_time",
                        "Find free slots on the user's calendar on a date.",
                        &[("date", "string"), ("duration_minutes", "integer")],
                    ),
                    near(
                        "create_task",
                        "Add a task to the user's task list with an optional due date.",
                        &[("title", "string"), ("due", "string")],
                    ),
                    near(
                        "send_calendar_invite",
                        "Email an invitation for an existing event to attendees.",
                        &[("event_id", "string"), ("attendees", "array")],
                    ),
                    near(
                        "list_calendars",
                        "List the calendars the user has access to.",
                        &[("account", "string")],
                    ),
                    near(
                        "set_alarm",
                        "Set an alarm on the user's phone.",
                        &[("time", "string"), ("label", "string")],
                    ),
                ]
            },
        },
        Task {
            name: "package",
            user: "Where is my package with tracking number 1Z999AA10123456784?",
            target: || {
                stub(
                    "track_package",
                    "Look up the location and status of a shipment by tracking number.",
                    json!({ "tracking_number": { "type": "string" } }),
                    json!({ "status": "in transit", "location": "Leipzig hub" }),
                )
            },
            pointer: "/tracking_number",
            needle: "1z999aa10123456784",
            distractors: || {
                vec![
                    near(
                        "lookup_order",
                        "Look up an online order by order number.",
                        &[("order_number", "string")],
                    ),
                    near(
                        "estimate_delivery_date",
                        "Estimate the delivery date for a shipment between two postcodes.",
                        &[("origin", "string"), ("destination", "string")],
                    ),
                    near(
                        "list_recent_shipments",
                        "List the user's recent inbound and outbound shipments.",
                        &[("days", "integer")],
                    ),
                    near(
                        "create_shipment",
                        "Create a new outbound shipment and get a label.",
                        &[("destination", "string"), ("weight_kg", "number")],
                    ),
                    near(
                        "get_shipping_rates",
                        "Get shipping rates between two addresses.",
                        &[("origin", "string"), ("destination", "string")],
                    ),
                    near(
                        "schedule_pickup",
                        "Schedule a carrier pickup at the user's address.",
                        &[("date", "string")],
                    ),
                    near(
                        "cancel_shipment",
                        "Cancel a shipment that has not yet been picked up.",
                        &[("shipment_id", "string")],
                    ),
                    near(
                        "validate_address",
                        "Validate and normalize a postal address.",
                        &[("address", "string")],
                    ),
                    near(
                        "file_delivery_claim",
                        "File a lost or damaged delivery claim for a shipment.",
                        &[("shipment_id", "string"), ("reason", "string")],
                    ),
                ]
            },
        },
    ]
}

/// The experiment's catalog: `n` tools with the target at position 3 of 5
/// or 6 of 10, or the first `n` near-misses without it.
fn catalog(task: &Task, n: usize, with_target: bool) -> Vec<Value> {
    let distractors = (task.distractors)();
    if !with_target {
        return distractors.into_iter().take(n).collect();
    }
    let mut items: Vec<Value> = distractors.into_iter().take(n - 1).collect();
    let pos = if n <= 5 { 2 } else { 5 };
    items.insert(pos, (task.target)());
    items
}

fn name_of(stub: &Value) -> String {
    stub["name"].as_str().unwrap_or_default().to_string()
}

/// The user's turn: the task, and the instruction to search, standing in
/// for the experiment's system prompt ("call tool_search to find it").
fn turn(task: &Task) -> String {
    format!(
        "{} You have no tool for this yet: search the tool catalog with tools_list, passing \
         a short query, then call the tool it finds.",
        task.user
    )
}

/// Case ids, fixed so `--case` filters stay stable.
fn id(task: &str, condition: &str) -> &'static str {
    match (task, condition) {
        ("weather", "n5") => "late-binding-weather-n5",
        ("weather", "n10") => "late-binding-weather-n10",
        ("weather", "missing") => "late-binding-weather-missing-n5",
        ("calendar", "n5") => "late-binding-calendar-n5",
        ("calendar", "n10") => "late-binding-calendar-n10",
        ("calendar", "missing") => "late-binding-calendar-missing-n5",
        ("package", "n5") => "late-binding-package-n5",
        ("package", "n10") => "late-binding-package-n10",
        _ => "late-binding-package-missing-n5",
    }
}

/// Scenario 10's cases: three tasks, each at five and ten near-misses with
/// the target, and at five without it.
pub(crate) fn cases() -> Vec<ModelCase> {
    let mut out = Vec::new();
    for task in tasks() {
        for (condition, n) in [("n5", 5), ("n10", 10)] {
            let mut case = ModelCase::new(
                id(task.name, condition),
                "Scenario 10: a tool found by tools_list among near-misses is called by \
                 append, with the tool block unchanged across the run",
            )
            .with_harness(bounded_harness())
            .with_num_ctx(LATE_BINDING_NUM_CTX)
            .keeping(BASE)
            .ask(turn(&task));
            for tool in catalog(&task, n, true) {
                case = case.with_tool(tool);
            }
            let target = name_of(&(task.target)());
            out.push(
                case.expect(Assertion::NoRunError)
                    .expect(Assertion::ToolCalled(target.clone()))
                    .expect(Assertion::ToolArgContains {
                        tool: target,
                        pointer: task.pointer.into(),
                        needle: task.needle.into(),
                    })
                    .expect(Assertion::Compacted(false))
                    .expect(Assertion::ToolBlockUnchanged { min_requests: 3 }),
            );
        }

        let mut missing = ModelCase::new(
            id(task.name, "missing"),
            "Scenario 10, host validation: with the target absent the search finds \
             nothing and no near-miss is called in its place",
        )
        .with_harness(bounded_harness())
        .with_num_ctx(LATE_BINDING_NUM_CTX)
        .keeping(BASE)
        .ask(turn(&task))
        .expect(Assertion::NoRunError)
        .expect(Assertion::ToolCalled("tools_list".into()))
        .expect(Assertion::FinalNonEmpty)
        .expect(Assertion::ToolBlockUnchanged { min_requests: 2 });
        for tool in catalog(&task, 5, false) {
            missing = missing
                .expect(Assertion::ToolNotCalled(name_of(&tool)))
                .with_tool(tool);
        }
        out.push(missing);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_matrix_is_the_experiments() {
        let cases = cases();
        assert_eq!(cases.len(), 9);
        let ids: Vec<&str> = cases.iter().map(|c| c.id).collect();
        assert!(ids.iter().all(|id| id.starts_with("late-binding-")));
        let mut unique = ids.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 9, "{ids:?}");

        for case in &cases {
            let tools = case.stubs["tools"].as_array().unwrap();
            // The catalog is registered, never declared from turn 0.
            assert!(tools.iter().all(|t| t["visible"] == false), "{}", case.id);
            assert_eq!(case.stubs["keep"], json!(BASE));
            let n = if case.id.ends_with("n10") { 10 } else { 5 };
            assert_eq!(tools.len(), n, "{}", case.id);
        }
        // Position 3 of 5 and 6 of 10, as in late_binding_catalog.py.
        let weather5 = &cases[0].stubs["tools"];
        assert_eq!(weather5[2]["name"], "get_weather");
        let weather10 = &cases[1].stubs["tools"];
        assert_eq!(weather10[5]["name"], "get_weather");
        assert!(!cases[2].stubs.to_string().contains("\"get_weather\""));
    }

    /// The host's side of each catalog (what a search over it finds) is
    /// unit-tested on these catalogs in `rustykrab-tools`' `tool_catalog`;
    /// here, only that each case holds or lacks its target as named.
    #[test]
    fn each_target_is_in_its_catalog_and_absent_from_the_missing_one() {
        for task in tasks() {
            let with: Vec<String> = catalog(&task, 10, true).iter().map(name_of).collect();
            let target = name_of(&(task.target)());
            assert!(with.contains(&target));
            let without: Vec<String> = catalog(&task, 5, false).iter().map(name_of).collect();
            assert!(!without.contains(&target));
        }
    }
}
