use async_trait::async_trait;
use calamine::{open_workbook_auto, Data, Reader};
use openclaw_core::types::ToolSchema;
use openclaw_core::{Error, Result, Tool};
use serde_json::{json, Value};

use crate::security;

/// A built-in tool that reads spreadsheet files (xlsx, xls, ods).
///
/// Uses the `calamine` crate for pure-Rust parsing — no external
/// dependencies required. Returns sheet data as JSON arrays.
pub struct SpreadsheetTool;

impl SpreadsheetTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SpreadsheetTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for SpreadsheetTool {
    fn name(&self) -> &str {
        "spreadsheet_read"
    }

    fn description(&self) -> &str {
        "Read data from spreadsheet files (xlsx, xls, ods). Returns sheet names and cell data as JSON arrays."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the spreadsheet file (.xlsx, .xls, or .ods)"
                    },
                    "sheet": {
                        "type": "string",
                        "description": "Name of the sheet to read. If omitted, reads the first sheet."
                    },
                    "max_rows": {
                        "type": "integer",
                        "description": "Maximum number of rows to return (default: 500). Use this to limit output size for large spreadsheets."
                    }
                },
                "required": ["path"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing path".into()))?;

        let safe_path = security::validate_path(path)
            .map_err(|e| Error::ToolExecution(format!("path rejected: {e}").into()))?;

        let sheet_name = args["sheet"].as_str().map(|s| s.to_string());
        let max_rows = args["max_rows"].as_u64().unwrap_or(500) as usize;

        // calamine is synchronous — run in a blocking thread
        let safe_path_clone = safe_path.clone();
        let result = tokio::task::spawn_blocking(move || {
            read_spreadsheet(&safe_path_clone, sheet_name.as_deref(), max_rows)
        })
        .await
        .map_err(|e| Error::ToolExecution(format!("task join error: {e}").into()))?
        .map_err(|e| Error::ToolExecution(e.into()))?;

        Ok(result)
    }
}

fn read_spreadsheet(
    path: &std::path::Path,
    sheet_name: Option<&str>,
    max_rows: usize,
) -> std::result::Result<Value, String> {
    let mut workbook = open_workbook_auto(path)
        .map_err(|e| format!("failed to open spreadsheet: {e}"))?;

    let sheet_names: Vec<String> = workbook.sheet_names().to_vec();

    if sheet_names.is_empty() {
        return Ok(json!({
            "path": path.display().to_string(),
            "sheets": [],
            "error": "no sheets found in workbook"
        }));
    }

    // Determine which sheet to read
    let target_sheet = match sheet_name {
        Some(name) => {
            if !sheet_names.contains(&name.to_string()) {
                return Ok(json!({
                    "path": path.display().to_string(),
                    "sheets": sheet_names,
                    "error": format!("sheet '{}' not found. Available sheets: {:?}", name, sheet_names)
                }));
            }
            name.to_string()
        }
        None => sheet_names[0].clone(),
    };

    let range = workbook
        .worksheet_range(&target_sheet)
        .map_err(|e| format!("failed to read sheet '{}': {e}", target_sheet))?;

    let total_rows = range.height();
    let total_cols = range.width();
    let rows_to_read = max_rows.min(total_rows);

    let mut rows: Vec<Vec<Value>> = Vec::with_capacity(rows_to_read);

    for row in range.rows().take(rows_to_read) {
        let cells: Vec<Value> = row
            .iter()
            .map(|cell| match cell {
                Data::Empty => Value::Null,
                Data::String(s) => Value::String(s.clone()),
                Data::Float(f) => {
                    // Represent whole numbers as integers for cleaner output
                    if f.fract() == 0.0 && *f >= i64::MIN as f64 && *f <= i64::MAX as f64 {
                        json!(*f as i64)
                    } else {
                        json!(f)
                    }
                }
                Data::Int(i) => json!(i),
                Data::Bool(b) => Value::Bool(*b),
                Data::Error(e) => Value::String(format!("#ERR:{:?}", e)),
                Data::DateTime(dt) => Value::String(format!("{}", dt)),
                Data::DateTimeIso(s) => Value::String(s.clone()),
                Data::DurationIso(s) => Value::String(s.clone()),
            })
            .collect();
        rows.push(cells);
    }

    let truncated = total_rows > rows_to_read;

    Ok(json!({
        "path": path.display().to_string(),
        "sheet": target_sheet,
        "sheets": sheet_names,
        "total_rows": total_rows,
        "total_columns": total_cols,
        "rows_returned": rows_to_read,
        "truncated": truncated,
        "data": rows,
    }))
}
