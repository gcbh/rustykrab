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

use serde_json::Value;

use crate::error::ToolError;

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
}
