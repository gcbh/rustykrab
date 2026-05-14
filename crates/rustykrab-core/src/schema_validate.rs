//! Lightweight JSON-Schema validation for tool arguments.
//!
//! Walks the `parameters` fragment on a tool's [`crate::types::ToolSchema`]
//! and produces descriptive `InvalidInput` errors that include enum hints
//! and expected types, so a model that called a tool incorrectly can
//! self-correct on the next round without re-reading the full schema.
//!
//! Only the subset of JSON Schema actually used by tools in this workspace
//! is implemented: top-level `properties`, `required`, per-field `type`,
//! and `enum`. Nested object/array validation is intentionally skipped —
//! tools that need deeper checks keep doing them in `execute()`.

use serde_json::Value;

use crate::error::ToolError;

/// Validate `args` against the `parameters` fragment of a tool schema.
///
/// Returns an `InvalidInput` [`ToolError`] with a message the model can act
/// on (enumerating valid enum values, naming the expected type, etc.).
pub fn validate_tool_args(parameters: &Value, args: &Value) -> Result<(), ToolError> {
    let args_obj = match args {
        Value::Object(map) => map,
        Value::Null => {
            // Treat missing args as an empty object so the required-field
            // check below produces the right message.
            return validate_required(parameters, &serde_json::Map::new());
        }
        other => {
            return Err(ToolError::invalid_input(format!(
                "arguments must be a JSON object, got {}",
                describe_type(other)
            )));
        }
    };

    validate_required(parameters, args_obj)?;

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
    let Some(required) = parameters.get("required").and_then(Value::as_array) else {
        return Ok(());
    };
    let properties = parameters.get("properties").and_then(Value::as_object);

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
}
