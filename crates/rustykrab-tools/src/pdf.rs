use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, Tool};
use serde_json::{json, Value};

use crate::security;

/// A built-in tool that extracts text content from PDF files.
///
/// Security: Path traversal protection and sanitized inputs to
/// external commands (pdftotext, python3).
pub struct PdfTool;

impl PdfTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for PdfTool {
    fn default() -> Self {
        Self::new()
    }
}

/// Validate a page range string (e.g., "1-5", "3").
/// Only allows digits and a single dash.
fn validate_page_range(pages: &str) -> std::result::Result<(), String> {
    if pages.is_empty() {
        return Err("empty page range".into());
    }
    for ch in pages.chars() {
        if !ch.is_ascii_digit() && ch != '-' {
            return Err(format!("invalid character in page range: '{ch}'"));
        }
    }
    // Ensure at most one dash
    if pages.matches('-').count() > 1 {
        return Err("page range must have at most one dash (e.g., '1-5')".into());
    }
    Ok(())
}

#[async_trait]
impl Tool for PdfTool {
    fn name(&self) -> &str {
        "pdf"
    }

    fn description(&self) -> &str {
        "Extract text content from a PDF file. Supports locked/encrypted PDFs \
         with a password parameter. If pymupdf is not installed, it will fall \
         back to pdftotext or PyPDF2."
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
                        "description": "Path to the PDF file"
                    },
                    "pages": {
                        "type": "string",
                        "description": "Optional page range to extract (e.g. \"1-5\")"
                    },
                    "password": {
                        "type": "string",
                        "description": "Password for locked/encrypted PDFs"
                    }
                },
                "required": ["path"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| rustykrab_core::Error::ToolExecution("missing path".into()))?;

        // Validate path for traversal attacks
        let safe_path = security::validate_path(path).map_err(|e| {
            rustykrab_core::Error::ToolExecution(format!("path rejected: {e}").into())
        })?;

        let pages = args["pages"].as_str();
        let password = args["password"].as_str();

        // Validate page range if provided
        if let Some(p) = pages {
            validate_page_range(p).map_err(|e| {
                rustykrab_core::Error::ToolExecution(format!("invalid page range: {e}").into())
            })?;
        }

        let safe_path_str = safe_path.to_string_lossy();

        // Try pymupdf first (handles locked PDFs, best quality)
        let result = try_pymupdf(&safe_path_str, pages, password).await;

        let (content, pages_extracted) = match result {
            Ok(v) => v,
            Err(_) => {
                // Fallback to pdftotext (no password support)
                let result2 = try_pdftotext(&safe_path_str, pages).await;
                match result2 {
                    Ok(v) => v,
                    Err(_) => {
                        // Last resort: PyPDF2
                        try_python_pdf(&safe_path_str, pages)
                            .await
                            .map_err(|e| rustykrab_core::Error::ToolExecution(
                                format!("failed to extract PDF text (tried pymupdf, pdftotext, PyPDF2): {e}").into()
                            ))?
                    }
                }
            }
        };

        Ok(json!({
            "path": path,
            "content": content,
            "pages_extracted": pages_extracted,
        }))
    }
}

/// Extract text using pymupdf (formerly fitz). Handles encrypted/locked PDFs.
/// This is the preferred method — fast, accurate, and handles most PDF types.
async fn try_pymupdf(
    path: &str,
    pages: Option<&str>,
    password: Option<&str>,
) -> std::result::Result<(String, u64), String> {
    let escaped_path = path.replace('\\', "\\\\").replace('"', "\\\"");
    let escaped_password = password
        .map(|p| p.replace('\\', "\\\\").replace('"', "\\\""))
        .unwrap_or_default();
    let page_filter = pages.unwrap_or("");

    let script = format!(
        r#"
import sys
try:
    import pymupdf
except ImportError:
    try:
        import fitz as pymupdf
    except ImportError:
        print("pymupdf not installed", file=sys.stderr)
        sys.exit(1)

try:
    doc = pymupdf.open("{escaped_path}")
    password = "{escaped_password}"
    if password and doc.is_encrypted:
        if not doc.authenticate(password):
            print("Error: incorrect password", file=sys.stderr)
            sys.exit(1)
    elif doc.is_encrypted:
        print("Error: PDF is encrypted and no password was provided", file=sys.stderr)
        sys.exit(1)

    total = len(doc)
    page_range = "{page_filter}"
    if page_range:
        parts = page_range.split("-")
        start = int(parts[0]) - 1
        end = int(parts[1]) if len(parts) > 1 else start + 1
    else:
        start = 0
        end = total

    text = ""
    count = 0
    for i in range(start, min(end, total)):
        text += doc[i].get_text()
        text += "\n"
        count += 1
    print(text)
    print(f"PAGES:{{count}}", file=sys.stderr)
except Exception as e:
    print(f"Error: {{e}}", file=sys.stderr)
    sys.exit(1)
"#
    );

    // Use the same python resolution as code_execution
    let python = resolve_python();

    let output = tokio::process::Command::new(&python)
        .arg("-c")
        .arg(&script)
        .output()
        .await
        .map_err(|e| format!("python3 not available: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "pymupdf extraction failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let content = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let pages_extracted = stderr
        .lines()
        .find(|l| l.starts_with("PAGES:"))
        .and_then(|l| l.strip_prefix("PAGES:"))
        .and_then(|n| n.trim().parse::<u64>().ok())
        .unwrap_or(1);

    Ok((content, pages_extracted))
}

/// Resolve the user's python3 path (same logic as code_execution tool).
fn resolve_python() -> std::path::PathBuf {
    if let Ok(output) = std::process::Command::new("which").arg("python3").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                if let Ok(canonical) = std::fs::canonicalize(&path) {
                    return canonical;
                }
                return path.into();
            }
        }
    }
    "python3".into()
}

async fn try_pdftotext(
    path: &str,
    pages: Option<&str>,
) -> std::result::Result<(String, u64), String> {
    let mut cmd = tokio::process::Command::new("pdftotext");

    if let Some(page_range) = pages {
        // page_range already validated to contain only digits and dash
        let parts: Vec<&str> = page_range.split('-').collect();
        if let Some(first) = parts.first() {
            if !first.is_empty() {
                cmd.arg("-f").arg(first);
            }
        }
        if let Some(last) = parts.get(1) {
            if !last.is_empty() {
                cmd.arg("-l").arg(last);
            }
        }
    }

    cmd.arg(path).arg("-"); // output to stdout

    let output = cmd
        .output()
        .await
        .map_err(|e| format!("pdftotext not available: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "pdftotext failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let content = String::from_utf8_lossy(&output.stdout).to_string();
    let pages_extracted = content.matches('\x0c').count().max(1) as u64;

    Ok((content, pages_extracted))
}

async fn try_python_pdf(
    path: &str,
    pages: Option<&str>,
) -> std::result::Result<(String, u64), String> {
    // Sanitize path for Python string: escape backslashes and quotes
    let escaped_path = path.replace('\\', "\\\\").replace('"', "\\\"");

    // page_range is already validated to only contain digits and dash
    let page_filter = pages.unwrap_or("");

    let script = format!(
        r#"
import sys
try:
    import PyPDF2
    reader = PyPDF2.PdfReader("{escaped_path}")
    page_range = "{page_filter}"
    total = len(reader.pages)
    if page_range:
        parts = page_range.split("-")
        start = int(parts[0]) - 1
        end = int(parts[1]) if len(parts) > 1 else start + 1
    else:
        start = 0
        end = total
    text = ""
    count = 0
    for i in range(start, min(end, total)):
        text += reader.pages[i].extract_text() or ""
        text += "\n"
        count += 1
    print(text)
    print(f"PAGES:{{count}}", file=sys.stderr)
except Exception as e:
    print(f"Error: {{e}}", file=sys.stderr)
    sys.exit(1)
"#
    );

    let python = resolve_python();
    let output = tokio::process::Command::new(&python)
        .arg("-c")
        .arg(&script)
        .output()
        .await
        .map_err(|e| format!("python3 not available: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "python3 PDF extraction failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let content = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let pages_extracted = stderr
        .lines()
        .find(|l| l.starts_with("PAGES:"))
        .and_then(|l| l.strip_prefix("PAGES:"))
        .and_then(|n| n.trim().parse::<u64>().ok())
        .unwrap_or(1);

    Ok((content, pages_extracted))
}
