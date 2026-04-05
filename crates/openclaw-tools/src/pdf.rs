use async_trait::async_trait;
use openclaw_core::types::ToolSchema;
use openclaw_core::{Result, Tool};
use serde_json::{json, Value};

/// A built-in tool that extracts text content from PDF files.
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

#[async_trait]
impl Tool for PdfTool {
    fn name(&self) -> &str {
        "pdf"
    }

    fn description(&self) -> &str {
        "Extract text content from a PDF file."
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
                    }
                },
                "required": ["path"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| openclaw_core::Error::ToolExecution("missing path".into()))?;

        let pages = args["pages"].as_str();

        // Try pdftotext first
        let result = try_pdftotext(path, pages).await;

        let (content, pages_extracted) = match result {
            Ok(v) => v,
            Err(_) => {
                // Fallback to python3
                try_python_pdf(path, pages)
                    .await
                    .map_err(|e| openclaw_core::Error::ToolExecution(
                        format!("failed to extract PDF text (tried pdftotext and python3): {e}")
                    ))?
            }
        };

        Ok(json!({
            "path": path,
            "content": content,
            "pages_extracted": pages_extracted,
        }))
    }
}

async fn try_pdftotext(path: &str, pages: Option<&str>) -> std::result::Result<(String, u64), String> {
    let mut cmd = tokio::process::Command::new("pdftotext");

    if let Some(page_range) = pages {
        // pdftotext uses -f (first) and -l (last) flags
        let parts: Vec<&str> = page_range.split('-').collect();
        if let Some(first) = parts.first() {
            cmd.arg("-f").arg(first);
        }
        if let Some(last) = parts.get(1) {
            cmd.arg("-l").arg(last);
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

async fn try_python_pdf(path: &str, pages: Option<&str>) -> std::result::Result<(String, u64), String> {
    let page_filter = pages.unwrap_or("");
    let script = format!(
        r#"
import sys
try:
    import PyPDF2
    reader = PyPDF2.PdfReader("{path}")
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

    let output = tokio::process::Command::new("python3")
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
