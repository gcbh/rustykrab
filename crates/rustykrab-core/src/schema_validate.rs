//! Lightweight JSON-Schema validation for tool arguments.
//!
//! Walks the `parameters` fragment on a tool's [`crate::types::ToolSchema`]
//! and produces descriptive `InvalidInput` errors that include enum hints
//! and expected types, so a model that called a tool incorrectly can
//! self-correct on the next round without re-reading the full schema.
//!
//! Only the subset of JSON Schema actually used by tools in this workspace
//! is implemented: top-level `properties`, `required`, per-field `type` and
//! `enum`, plus `allOf` clauses with `if`/`then` conditional requirements.
//! Nested object/array validation is intentionally skipped — tools that need
//! deeper checks keep doing them in `execute()`.
//!
//! # Coercion before validation
//!
//! [`coerce_tool_args`] runs first, in the runner, and turns a string into
//! the integer, number or boolean its schema declares when the string is
//! exactly that value (`"60"` to `60`, `"2.5"` to `2.5`, `"true"` to
//! `true`). Qwen's XML tool-call format carries every parameter as text,
//! and Ollama's parser types a parameter only from a tool the request
//! declared, so a tool that reached the model by append (plan section 12)
//! arrives untyped: qwen3.8 found and called `create_calendar_event` 21
//! times with `duration_minutes: "30"` and was rejected every time, while
//! gemma4 sent a number. The host absorbs that model difference as data;
//! there is no per-model branch. A string that is not exactly a value of
//! the declared type is left alone, so validation rejects it as it always
//! did. No schema opts out: a value is only ever changed where its schema
//! already forbids a string, so a call that validated before is untouched.

use serde_json::Value;

use crate::error::ToolError;

/// One argument [`coerce_tool_args`] changed from a string to its schema's
/// type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coercion {
    /// Where the value sits in the arguments, as a JSON pointer
    /// (`/duration_minutes`, `/items/0/count`).
    pub pointer: String,
    /// The type it became: `integer`, `number` or `boolean`.
    pub to: &'static str,
}

/// Coerce string arguments to the scalar types their schema declares.
///
/// A string becomes an integer, a number or a boolean when the schema's
/// `type` for that field is one of those (or a list of types without
/// `string`) and the string, less surrounding whitespace, is exactly such a
/// value: an integer in canonical decimal form that fits 64 bits (`"60"`,
/// `"-3"`; not `"060"`, `"+6"`, `"6.0"` or `"6 min"`); a JSON number with
/// at most 15 significant digits, so the value the tool sees prints back as
/// the same number (`"2.5"`, `"1e3"`; not `".5"`, `"NaN"` or 20 digits of
/// pi); or `"true"` or `"false"`. Nested objects (`properties`,
/// `additionalProperties`) and arrays (`items`) are walked by the same
/// rule. Anything else is left as it is, for [`validate_tool_args`] to
/// judge.
///
/// Returns `None` when nothing needed coercing, the common case, without
/// copying the arguments; otherwise the coerced copy and what changed.
pub fn coerce_tool_args(parameters: &Value, args: &Value) -> Option<(Value, Vec<Coercion>)> {
    let mut found = Vec::new();
    collect_coercions(parameters, args, &mut String::new(), &mut found);
    if found.is_empty() {
        return None;
    }
    let mut coerced = args.clone();
    let mut changes = Vec::with_capacity(found.len());
    for (pointer, value, to) in found {
        if let Some(slot) = coerced.pointer_mut(&pointer) {
            *slot = value;
            changes.push(Coercion { pointer, to });
        }
    }
    Some((coerced, changes))
}

/// Walk `value` against `schema`, collecting each string that should become
/// a scalar: its pointer, its new value and the type named.
fn collect_coercions(
    schema: &Value,
    value: &Value,
    pointer: &mut String,
    out: &mut Vec<(String, Value, &'static str)>,
) {
    match value {
        Value::String(text) => {
            if let Some((typed, to)) = exact_scalar(schema, text) {
                out.push((pointer.clone(), typed, to));
            }
        }
        Value::Object(map) => {
            let properties = schema.get("properties").and_then(Value::as_object);
            let additional = schema.get("additionalProperties").filter(|a| a.is_object());
            for (key, field) in map {
                let Some(field_schema) = properties.and_then(|p| p.get(key)).or(additional) else {
                    continue;
                };
                let len = pointer.len();
                pointer.push('/');
                pointer.push_str(&key.replace('~', "~0").replace('/', "~1"));
                collect_coercions(field_schema, field, pointer, out);
                pointer.truncate(len);
            }
        }
        Value::Array(items) => {
            let Some(item_schema) = schema.get("items").filter(|i| i.is_object()) else {
                return;
            };
            for (index, item) in items.iter().enumerate() {
                let len = pointer.len();
                pointer.push('/');
                pointer.push_str(&index.to_string());
                collect_coercions(item_schema, item, pointer, out);
                pointer.truncate(len);
            }
        }
        _ => {}
    }
}

/// The value `text` exactly is, under the first scalar type `schema`
/// declares, and that type's name. `None` when the schema declares no
/// integer, number or boolean type, also allows a string, or `text` is not
/// exactly a value of it.
fn exact_scalar(schema: &Value, text: &str) -> Option<(Value, &'static str)> {
    let types: Vec<&str> = match schema.get("type")? {
        Value::String(t) => vec![t.as_str()],
        Value::Array(ts) => ts.iter().filter_map(Value::as_str).collect(),
        _ => return None,
    };
    if types.contains(&"string") {
        return None;
    }
    let text = text.trim();
    types.into_iter().find_map(|t| match t {
        "integer" => exact_integer(text).map(|v| (v, "integer")),
        "number" => exact_number(text).map(|v| (v, "number")),
        "boolean" => match text {
            "true" => Some((Value::Bool(true), "boolean")),
            "false" => Some((Value::Bool(false), "boolean")),
            _ => None,
        },
        _ => None,
    })
}

/// An integer in canonical decimal form (it prints back as the same text)
/// that fits `i64` or `u64`.
fn exact_integer(text: &str) -> Option<Value> {
    if let Ok(n) = text.parse::<i64>() {
        return (n.to_string() == text).then(|| Value::from(n));
    }
    let n = text.parse::<u64>().ok()?;
    (n.to_string() == text).then(|| Value::from(n))
}

/// Most significant digits a decimal can have and still come back from an
/// `f64` unchanged (`f64::DIGITS`).
const EXACT_DECIMAL_DIGITS: usize = f64::DIGITS as usize;

/// A JSON number (so no `+`, no bare `.5`, no `NaN`) that loses nothing on
/// the way to the tool: an exact integer, or a finite decimal with at most
/// [`EXACT_DECIMAL_DIGITS`] significant digits.
fn exact_number(text: &str) -> Option<Value> {
    if let Some(integer) = exact_integer(text) {
        return Some(integer);
    }
    let number: serde_json::Number = serde_json::from_str(text).ok()?;
    if number.as_f64().is_none_or(|f| !f.is_finite()) {
        return None;
    }
    let mantissa = text.split(['e', 'E']).next().unwrap_or(text);
    if !mantissa.contains('.') && !text.contains(['e', 'E']) {
        // An integer too large for 64 bits would arrive rounded.
        return None;
    }
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let significant = digits.trim_start_matches('0').trim_end_matches('0').len();
    (significant <= EXACT_DECIMAL_DIGITS).then_some(Value::Number(number))
}

/// Validate `args` against the `parameters` fragment of a tool schema.
///
/// Returns an `InvalidInput` [`ToolError`] with a message the model can act
/// on (enumerating valid enum values, naming the expected type, etc.).
pub fn validate_tool_args(parameters: &Value, args: &Value) -> Result<(), ToolError> {
    let empty_args = serde_json::Map::new();
    let args_obj = match args {
        Value::Object(map) => map,
        // Treat missing args as an empty object so required-field checks
        // produce the same useful message as an explicit `{}`.
        Value::Null => &empty_args,
        other => {
            return Err(ToolError::invalid_input(format!(
                "arguments must be a JSON object, got {}",
                describe_type(other)
            )));
        }
    };

    validate_required(parameters, args_obj)?;
    validate_conditional_requirements(parameters, args_obj)?;

    if let Some(properties) = parameters.get("properties").and_then(Value::as_object) {
        for (field_name, field_value) in args_obj {
            let Some(field_schema) = properties.get(field_name) else {
                continue;
            };
            validate_field(field_name, field_schema, field_value)?;
        }
    }

    Ok(())
}

fn validate_required(
    parameters: &Value,
    args_obj: &serde_json::Map<String, Value>,
) -> Result<(), ToolError> {
    validate_required_with_properties(
        parameters,
        args_obj,
        parameters.get("properties").and_then(Value::as_object),
    )
}

fn validate_required_with_properties(
    schema: &Value,
    args_obj: &serde_json::Map<String, Value>,
    fallback_properties: Option<&serde_json::Map<String, Value>>,
) -> Result<(), ToolError> {
    let Some(required) = schema.get("required").and_then(Value::as_array) else {
        return Ok(());
    };
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .or(fallback_properties);

    for req in required {
        let Some(name) = req.as_str() else { continue };
        if args_obj.contains_key(name) {
            continue;
        }

        let field_schema = properties.and_then(|p| p.get(name));
        let hint = field_schema
            .map(describe_field_expectation)
            .unwrap_or_default();
        let msg = if hint.is_empty() {
            format!("missing required field '{name}'")
        } else {
            format!("missing required field '{name}' ({hint})")
        };
        return Err(ToolError::invalid_input(msg));
    }
    Ok(())
}

/// Apply the conditional required-field clauses used by polymorphic tools.
///
/// This deliberately implements a small, predictable JSON-Schema subset:
/// each `allOf` entry may contain `if` plus `then`/`else`, and conditions may
/// inspect `required`, `properties.const`, `properties.enum`, and
/// `properties.type`. That covers the schemas currently emitted by the
/// workspace without turning tool dispatch into a second schema engine.
fn validate_conditional_requirements(
    parameters: &Value,
    args_obj: &serde_json::Map<String, Value>,
) -> Result<(), ToolError> {
    let Some(clauses) = parameters.get("allOf").and_then(Value::as_array) else {
        return Ok(());
    };
    let root_properties = parameters.get("properties").and_then(Value::as_object);

    for clause in clauses {
        if let Some(condition) = clause.get("if") {
            let branch = if condition_matches(condition, args_obj) {
                clause.get("then")
            } else {
                clause.get("else")
            };
            if let Some(branch) = branch {
                validate_required_with_properties(branch, args_obj, root_properties)?;
            }
        } else {
            validate_required_with_properties(clause, args_obj, root_properties)?;
        }
    }

    Ok(())
}

fn condition_matches(condition: &Value, args_obj: &serde_json::Map<String, Value>) -> bool {
    if let Some(required) = condition.get("required").and_then(Value::as_array) {
        for field in required.iter().filter_map(Value::as_str) {
            if !args_obj.contains_key(field) {
                return false;
            }
        }
    }

    let Some(properties) = condition.get("properties").and_then(Value::as_object) else {
        return true;
    };
    for (name, field_schema) in properties {
        // JSON Schema's `properties` keyword does not itself require a field.
        let Some(value) = args_obj.get(name) else {
            continue;
        };
        if let Some(expected) = field_schema.get("const") {
            if value != expected {
                return false;
            }
        }
        if let Some(allowed) = field_schema.get("enum").and_then(Value::as_array) {
            if !allowed.iter().any(|candidate| candidate == value) {
                return false;
            }
        }
        if let Some(expected_type) = field_schema.get("type").and_then(Value::as_str) {
            if !value_matches_type(value, expected_type) {
                return false;
            }
        }
    }
    true
}

fn validate_field(name: &str, field_schema: &Value, value: &Value) -> Result<(), ToolError> {
    // `enum` takes precedence: if the schema constrains the value to a
    // closed set, mention the allowed set explicitly. This is the most
    // important case for polymorphic tools (e.g. `action`).
    if let Some(enum_values) = field_schema.get("enum").and_then(Value::as_array) {
        if !enum_values.iter().any(|v| v == value) {
            let allowed = format_enum(enum_values);
            let got = format_value(value);
            return Err(ToolError::invalid_input(format!(
                "invalid value for '{name}': {got} — expected one of: {allowed}"
            )));
        }
        // If the value passed the enum check, the type is implicitly fine.
        return Ok(());
    }

    if let Some(expected) = field_schema.get("type").and_then(Value::as_str) {
        if !value_matches_type(value, expected) {
            return Err(ToolError::invalid_input(format!(
                "field '{name}' must be {}, got {}",
                expected,
                describe_type(value),
            )));
        }
    }

    Ok(())
}

fn value_matches_type(value: &Value, expected: &str) -> bool {
    match expected {
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.is_i64() || value.is_u64(),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "null" => value.is_null(),
        // Unknown type keyword: don't reject — let the tool handle it.
        _ => true,
    }
}

fn describe_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Render a short description of what a field should look like, used in the
/// "missing required field" message. Prefers enum listings, then type.
fn describe_field_expectation(field_schema: &Value) -> String {
    if let Some(enum_values) = field_schema.get("enum").and_then(Value::as_array) {
        return format!("expected one of: {}", format_enum(enum_values));
    }
    if let Some(t) = field_schema.get("type").and_then(Value::as_str) {
        return format!("expected {t}");
    }
    String::new()
}

fn format_enum(values: &[Value]) -> String {
    values
        .iter()
        .map(format_value)
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_value(value: &Value) -> String {
    match value {
        Value::String(s) => format!("'{s}'"),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cron_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "list", "delete", "list_runs"],
                    "description": "The action to perform"
                },
                "schedule": { "type": "string" },
                "limit": { "type": "integer" }
            },
            "required": ["action"]
        })
    }

    #[test]
    fn missing_required_field_with_enum_hint() {
        let err = validate_tool_args(&cron_schema(), &json!({})).unwrap_err();
        assert_eq!(err.kind, crate::error::ToolErrorKind::InvalidInput);
        assert!(
            err.message.contains("missing required field 'action'"),
            "got: {}",
            err.message
        );
        assert!(
            err.message.contains("'create'") && err.message.contains("'list_runs'"),
            "should enumerate enum values, got: {}",
            err.message
        );
    }

    #[test]
    fn missing_required_field_with_type_hint() {
        let schema = json!({
            "type": "object",
            "properties": { "url": { "type": "string" } },
            "required": ["url"]
        });
        let err = validate_tool_args(&schema, &json!({})).unwrap_err();
        assert!(err.message.contains("'url'"), "got: {}", err.message);
        assert!(err.message.contains("string"), "got: {}", err.message);
    }

    #[test]
    fn invalid_enum_value_lists_alternatives() {
        let args = json!({ "action": "crate" });
        let err = validate_tool_args(&cron_schema(), &args).unwrap_err();
        assert!(
            err.message.contains("invalid value for 'action'"),
            "got: {}",
            err.message
        );
        assert!(err.message.contains("'crate'"), "got: {}", err.message);
        assert!(err.message.contains("'create'"), "got: {}", err.message);
        assert!(err.message.contains("'list_runs'"), "got: {}", err.message);
    }

    #[test]
    fn wrong_type_reports_actual_and_expected() {
        let args = json!({ "action": "list", "limit": "twenty" });
        let err = validate_tool_args(&cron_schema(), &args).unwrap_err();
        assert!(
            err.message.contains("'limit'") && err.message.contains("integer"),
            "got: {}",
            err.message
        );
        assert!(err.message.contains("string"), "got: {}", err.message);
    }

    #[test]
    fn integer_accepts_both_signed_and_unsigned() {
        let schema = json!({
            "type": "object",
            "properties": { "n": { "type": "integer" } }
        });
        validate_tool_args(&schema, &json!({ "n": 42 })).unwrap();
        validate_tool_args(&schema, &json!({ "n": -1 })).unwrap();
    }

    #[test]
    fn unknown_fields_pass_through() {
        // Tools accept extra fields silently — validator must not reject them.
        let args = json!({ "action": "create", "extra_thing": "ignored" });
        validate_tool_args(&cron_schema(), &args).unwrap();
    }

    #[test]
    fn null_args_treated_as_missing_object() {
        let err = validate_tool_args(&cron_schema(), &Value::Null).unwrap_err();
        assert!(
            err.message.contains("missing required field 'action'"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn non_object_args_rejected() {
        let err = validate_tool_args(&cron_schema(), &json!("hello")).unwrap_err();
        assert!(
            err.message.contains("arguments must be a JSON object"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn schema_without_required_passes_empty_args() {
        let schema = json!({
            "type": "object",
            "properties": { "category": { "type": "string" } }
        });
        validate_tool_args(&schema, &json!({})).unwrap();
    }

    #[test]
    fn valid_call_succeeds() {
        let args = json!({ "action": "create", "schedule": "0 9 * * *" });
        validate_tool_args(&cron_schema(), &args).unwrap();
    }

    #[test]
    fn conditional_required_fields_are_enforced() {
        let schema = json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["act", "evaluate", "snapshot"] },
                "actAction": { "type": "string", "enum": ["click", "type"] },
                "expression": { "type": "string" }
            },
            "required": ["action"],
            "allOf": [
                {
                    "if": { "properties": { "action": { "const": "act" } }, "required": ["action"] },
                    "then": { "required": ["actAction"] }
                },
                {
                    "if": { "properties": { "action": { "const": "evaluate" } }, "required": ["action"] },
                    "then": { "required": ["expression"] }
                }
            ]
        });

        let act_err = validate_tool_args(&schema, &json!({ "action": "act" })).unwrap_err();
        assert_eq!(act_err.kind, crate::error::ToolErrorKind::InvalidInput);
        assert!(act_err.message.contains("'actAction'"), "{act_err}");
        assert!(act_err.message.contains("'click'"), "{act_err}");

        let eval_err = validate_tool_args(&schema, &json!({ "action": "evaluate" })).unwrap_err();
        assert!(eval_err.message.contains("'expression'"), "{eval_err}");

        validate_tool_args(&schema, &json!({ "action": "snapshot" })).unwrap();
        validate_tool_args(&schema, &json!({ "action": "act", "actAction": "click" })).unwrap();
    }

    #[test]
    fn conditional_enum_matches_multiple_values() {
        let schema = json!({
            "type": "object",
            "properties": {
                "action": { "type": "string" },
                "actAction": { "type": "string" },
                "text": { "type": "string" }
            },
            "required": ["action"],
            "allOf": [{
                "if": {
                    "properties": {
                        "action": { "const": "act" },
                        "actAction": { "enum": ["type", "fill"] }
                    },
                    "required": ["action", "actAction"]
                },
                "then": { "required": ["text"] }
            }]
        });

        let err = validate_tool_args(&schema, &json!({ "action": "act", "actAction": "fill" }))
            .unwrap_err();
        assert!(err.message.contains("'text'"), "{err}");
        validate_tool_args(&schema, &json!({ "action": "act", "actAction": "click" })).unwrap();
    }

    // ── coercion ─────────────────────────────────────────────────────────

    /// Scenario 10's calendar target, which qwen3.8 called with every
    /// parameter as text.
    fn calendar_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": { "type": "string" },
                "start": { "type": "string" },
                "duration_minutes": { "type": "integer" },
                "all_day": { "type": "boolean" },
                "hours": { "type": "number" },
            },
            "required": ["title"]
        })
    }

    fn coerced(schema: &Value, args: Value) -> (Value, Vec<Coercion>) {
        coerce_tool_args(schema, &args).expect("something to coerce")
    }

    #[test]
    fn an_exact_integer_string_becomes_an_integer_and_passes_validation() {
        let args = json!({ "title": "Dentist", "duration_minutes": "30" });
        let err = validate_tool_args(&calendar_schema(), &args).unwrap_err();
        assert_eq!(
            err.message,
            "field 'duration_minutes' must be integer, got string"
        );

        let (args, changes) = coerced(&calendar_schema(), args);
        assert_eq!(args, json!({ "title": "Dentist", "duration_minutes": 30 }));
        assert_eq!(
            changes,
            [Coercion {
                pointer: "/duration_minutes".into(),
                to: "integer"
            }]
        );
        validate_tool_args(&calendar_schema(), &args).unwrap();

        for (text, n) in [("60", 60i64), ("-3", -3), ("0", 0), (" 45\n", 45)] {
            let (args, _) = coerced(&calendar_schema(), json!({ "duration_minutes": text }));
            assert_eq!(args["duration_minutes"], json!(n), "{text:?}");
        }
        let (args, _) = coerced(
            &calendar_schema(),
            json!({ "duration_minutes": "18446744073709551615" }),
        );
        assert_eq!(args["duration_minutes"], json!(u64::MAX));
    }

    #[test]
    fn exact_number_and_boolean_strings_become_their_types() {
        let (args, changes) = coerced(
            &calendar_schema(),
            json!({ "hours": "2.5", "all_day": "false", "title": "true" }),
        );
        assert_eq!(args["hours"], json!(2.5));
        assert_eq!(args["all_day"], json!(false));
        // A string field keeps its string, whatever it looks like.
        assert_eq!(args["title"], json!("true"));
        assert_eq!(changes.len(), 2);
        validate_tool_args(&calendar_schema(), &args).unwrap();

        for (text, n) in [
            ("60", json!(60)),
            ("-0.25", json!(-0.25)),
            ("1e3", json!(1000.0)),
        ] {
            let (args, _) = coerced(&calendar_schema(), json!({ "hours": text }));
            assert_eq!(args["hours"], n, "{text:?}");
        }
        let (args, _) = coerced(&calendar_schema(), json!({ "all_day": "true" }));
        assert_eq!(args["all_day"], json!(true));
    }

    #[test]
    fn a_string_that_is_not_exactly_the_type_is_left_for_validation_to_reject() {
        for (field, text) in [
            ("duration_minutes", "30 minutes"),
            ("duration_minutes", "060"),
            ("duration_minutes", "+30"),
            ("duration_minutes", "30.0"),
            ("duration_minutes", "-0"),
            ("duration_minutes", "99999999999999999999"),
            ("duration_minutes", ""),
            ("hours", ".5"),
            ("hours", "2.5h"),
            ("hours", "NaN"),
            ("hours", "inf"),
            ("hours", "3.14159265358979323846"),
            ("hours", "123456789012345678901234"),
            ("all_day", "True"),
            ("all_day", "yes"),
            ("all_day", "1"),
        ] {
            let args = json!({ "title": "x", field: text });
            assert_eq!(
                coerce_tool_args(&calendar_schema(), &args),
                None,
                "{field}: {text:?}"
            );
            let err = validate_tool_args(&calendar_schema(), &args).unwrap_err();
            assert!(
                err.message
                    .starts_with(&format!("field '{field}' must be ")),
                "{field}: {text:?}: {err}"
            );
        }
        // Already typed, or no type to go by: nothing to do.
        assert_eq!(
            coerce_tool_args(&calendar_schema(), &json!({ "duration_minutes": 30 })),
            None
        );
        let untyped = json!({ "type": "object", "properties": { "n": {} } });
        assert_eq!(coerce_tool_args(&untyped, &json!({ "n": "3" })), None);
        let unknown = json!({ "type": "object", "properties": {} });
        assert_eq!(coerce_tool_args(&unknown, &json!({ "n": "3" })), None);
    }

    #[test]
    fn a_type_list_with_string_is_never_coerced_and_one_without_is() {
        let schema = json!({
            "type": "object",
            "properties": {
                "either": { "type": ["string", "integer"] },
                "maybe": { "type": ["integer", "null"] },
            }
        });
        let (args, changes) = coerced(&schema, json!({ "either": "5", "maybe": "5" }));
        assert_eq!(args, json!({ "either": "5", "maybe": 5 }));
        assert_eq!(changes.len(), 1);
    }

    #[test]
    fn nested_objects_and_arrays_are_coerced_by_the_same_rule() {
        let schema = json!({
            "type": "object",
            "properties": {
                "event": {
                    "type": "object",
                    "properties": {
                        "duration_minutes": { "type": "integer" },
                        "reminders": {
                            "type": "array",
                            "items": { "type": "integer" }
                        }
                    }
                },
                "attendees": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": { "optional": { "type": "boolean" } }
                    }
                },
                "limits": {
                    "type": "object",
                    "additionalProperties": { "type": "number" }
                },
                "a/b": { "type": "integer" }
            }
        });
        let args = json!({
            "event": { "duration_minutes": "30", "reminders": ["10", "later", 5] },
            "attendees": [ { "optional": "true" }, { "optional": "maybe" } ],
            "limits": { "cpu": "0.5" },
            "a/b": "7"
        });
        let (args, changes) = coerced(&schema, args);
        assert_eq!(
            args,
            json!({
                "event": { "duration_minutes": 30, "reminders": [10, "later", 5] },
                "attendees": [ { "optional": true }, { "optional": "maybe" } ],
                "limits": { "cpu": 0.5 },
                "a/b": 7
            })
        );
        let mut pointers: Vec<&str> = changes.iter().map(|c| c.pointer.as_str()).collect();
        pointers.sort();
        assert_eq!(
            pointers,
            [
                "/attendees/0/optional",
                "/a~1b",
                "/event/duration_minutes",
                "/event/reminders/0",
                "/limits/cpu"
            ]
        );
    }

    #[test]
    fn coercion_satisfies_an_integer_enum() {
        let schema = json!({
            "type": "object",
            "properties": { "level": { "type": "integer", "enum": [1, 2, 3] } }
        });
        let (args, _) = coerced(&schema, json!({ "level": "2" }));
        validate_tool_args(&schema, &args).unwrap();
        let (args, _) = coerced(&schema, json!({ "level": "4" }));
        let err = validate_tool_args(&schema, &args).unwrap_err();
        assert!(err.message.contains("expected one of: 1, 2, 3"), "{err}");
    }
}
