use async_trait::async_trait;
use chrono::Utc;
use openclaw_core::error::Result;
use openclaw_core::model::{ModelProvider, ModelResponse, StopReason, StreamEvent, Usage};
use openclaw_core::types::{Message, MessageContent, Role, ToolCall, ToolSchema};
use openclaw_core::Error;
use serde::Deserialize;
use uuid::Uuid;

/// Model provider that shells out to the `claude` CLI (Claude Code).
///
/// Uses the user's existing Claude subscription (Pro/Max/Team/Enterprise)
/// instead of requiring an API key.  The CLI is invoked in single-turn
/// print mode (`claude -p --output-format json`).
///
/// Tool calling is handled via prompt engineering: the model is instructed
/// to output a JSON `{"tool_calls": [...]}` object when it wants to invoke
/// tools, and the provider parses that from the response text.
///
/// # Setup
///
/// 1. Install Claude Code: `npm install -g @anthropic-ai/claude-code`
/// 2. Authenticate: `claude login`
/// 3. Set provider: `OPENCLAW_PROVIDER=claude-cli`
/// 4. Optionally set model: `CLAUDE_CLI_MODEL=claude-sonnet-4-20250514`
pub struct ClaudeCliProvider {
    /// Model to pass via `--model`. If `None`, uses the CLI default.
    model: Option<String>,
    /// Maximum tokens for the response.
    max_tokens: u32,
    /// Path to the `claude` binary. Defaults to `"claude"` (found via PATH).
    binary: String,
}

impl ClaudeCliProvider {
    pub fn new() -> Self {
        Self {
            model: None,
            max_tokens: 16384,
            binary: "claude".to_string(),
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    pub fn with_binary(mut self, binary: impl Into<String>) -> Self {
        self.binary = binary.into();
        self
    }

    /// Format the full conversation + tool schemas into a single prompt for
    /// the CLI's `-p` flag.
    fn build_prompt(messages: &[Message], tools: &[ToolSchema]) -> String {
        let mut prompt = String::with_capacity(4096);

        // System instructions for tool calling (only if tools are provided).
        if !tools.is_empty() {
            prompt.push_str("<system>\n");
            prompt.push_str("You have access to the following tools:\n\n");

            let tool_defs: Vec<serde_json::Value> = tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    })
                })
                .collect();

            if let Ok(json) = serde_json::to_string_pretty(&tool_defs) {
                prompt.push_str(&json);
            }

            prompt.push_str("\n\n");
            prompt.push_str(
                "IMPORTANT TOOL CALLING INSTRUCTIONS:\n\
                 When you want to call one or more tools, you MUST respond with ONLY \
                 a raw JSON object in this exact format (no markdown, no explanation, no \
                 code fences, nothing else):\n\n\
                 {\"tool_calls\":[{\"name\":\"tool_name\",\"arguments\":{...}}]}\n\n\
                 You may include multiple entries in the array to call several tools at once.\n\
                 When you do NOT need to call a tool, respond normally with text.\n\
                 NEVER mix text and tool calls in the same response.\n",
            );
            prompt.push_str("</system>\n\n");
        }

        // Conversation history.
        for msg in messages {
            match msg.role {
                Role::System => {
                    if let MessageContent::Text(ref text) = msg.content {
                        prompt.push_str(&format!("<system>\n{text}\n</system>\n\n"));
                    }
                }
                Role::User => {
                    if let MessageContent::Text(ref text) = msg.content {
                        prompt.push_str(&format!("[User]\n{text}\n\n"));
                    }
                }
                Role::Assistant => match &msg.content {
                    MessageContent::Text(text) => {
                        prompt.push_str(&format!("[Assistant]\n{text}\n\n"));
                    }
                    MessageContent::ToolCall(call) => {
                        let json = serde_json::json!({
                            "tool_calls": [{
                                "name": call.name,
                                "arguments": call.arguments,
                            }]
                        });
                        prompt.push_str(&format!(
                            "[Assistant]\n{}\n\n",
                            serde_json::to_string(&json).unwrap_or_default()
                        ));
                    }
                    MessageContent::MultiToolCall(calls) => {
                        let tc: Vec<serde_json::Value> = calls
                            .iter()
                            .map(|c| {
                                serde_json::json!({
                                    "name": c.name,
                                    "arguments": c.arguments,
                                })
                            })
                            .collect();
                        let json = serde_json::json!({ "tool_calls": tc });
                        prompt.push_str(&format!(
                            "[Assistant]\n{}\n\n",
                            serde_json::to_string(&json).unwrap_or_default()
                        ));
                    }
                    _ => {}
                },
                Role::Tool => {
                    if let MessageContent::ToolResult(ref result) = msg.content {
                        prompt.push_str(&format!(
                            "[Tool Result (call_id={})]\n{}\n\n",
                            result.call_id,
                            serde_json::to_string_pretty(&result.output).unwrap_or_default()
                        ));
                    }
                }
            }
        }

        prompt
    }

    /// Parse the `result` field from the CLI JSON output into a ModelResponse.
    fn parse_response(result_text: &str, has_tools: bool) -> Result<ModelResponse> {
        // If tools are available, check if the response is a tool call JSON object.
        if has_tools {
            let trimmed = result_text.trim();

            // Try to parse as a tool call response.
            if let Some(calls) = try_parse_tool_calls(trimmed) {
                if !calls.is_empty() {
                    let content = if calls.len() == 1 {
                        MessageContent::ToolCall(calls.into_iter().next().unwrap())
                    } else {
                        MessageContent::MultiToolCall(calls)
                    };

                    return Ok(ModelResponse {
                        message: Message {
                            id: Uuid::new_v4(),
                            role: Role::Assistant,
                            content,
                            created_at: Utc::now(),
                        },
                        usage: Usage::default(),
                        stop_reason: StopReason::ToolUse,
                    });
                }
            }
        }

        // Plain text response.
        Ok(ModelResponse {
            message: Message {
                id: Uuid::new_v4(),
                role: Role::Assistant,
                content: MessageContent::Text(result_text.to_string()),
                created_at: Utc::now(),
            },
            usage: Usage::default(),
            stop_reason: StopReason::EndTurn,
        })
    }
}

#[async_trait]
impl ModelProvider for ClaudeCliProvider {
    fn name(&self) -> &str {
        "claude-cli"
    }

    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
    ) -> Result<ModelResponse> {
        let prompt = Self::build_prompt(messages, tools);

        let mut cmd = tokio::process::Command::new(&self.binary);
        cmd.arg("-p")
            .arg(&prompt)
            .arg("--output-format")
            .arg("json")
            .arg("--max-turns")
            .arg("1")
            .arg("--verbose");

        if let Some(ref model) = self.model {
            cmd.arg("--model").arg(model);
        }

        tracing::debug!(binary = %self.binary, model = ?self.model, "invoking Claude CLI");

        let output = cmd
            .output()
            .await
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    Error::ModelProvider(format!(
                        "Claude CLI not found at '{}'. Install with: \
                         npm install -g @anthropic-ai/claude-code",
                        self.binary
                    ))
                } else {
                    Error::ModelProvider(format!("failed to run Claude CLI: {e}"))
                }
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            return Err(Error::ModelProvider(format!(
                "Claude CLI exited with status {}: stderr={stderr}, stdout={stdout}",
                output.status
            )));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);

        // The JSON output may contain multiple JSON objects (one per line for
        // streaming events).  We want the last `{"type":"result",...}` line.
        let cli_resp = parse_cli_json(&stdout)?;

        if cli_resp.is_error {
            return Err(Error::ModelProvider(format!(
                "Claude CLI error: {}",
                cli_resp.result
            )));
        }

        let usage = Usage {
            prompt_tokens: 0,
            completion_tokens: 0,
        };

        let mut response = Self::parse_response(&cli_resp.result, !tools.is_empty())?;
        response.usage = usage;

        Ok(response)
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        on_event: &(dyn Fn(StreamEvent) + Send + Sync),
    ) -> Result<ModelResponse> {
        // For now, fall back to non-streaming and emit the full text at once.
        // A future improvement could use `--output-format stream-json` and
        // parse incremental events from stdout.
        let response = self.chat(messages, tools).await?;
        if let Some(text) = response.message.content.as_text() {
            on_event(StreamEvent::TextDelta(text.to_string()));
        }
        on_event(StreamEvent::Done(response.clone()));
        Ok(response)
    }
}

// ---------------------------------------------------------------------------
// CLI JSON output parsing
// ---------------------------------------------------------------------------

/// The JSON structure returned by `claude -p --output-format json`.
#[derive(Deserialize)]
#[allow(dead_code)]
struct CliResponse {
    #[serde(default)]
    r#type: String,
    result: String,
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    cost_usd: f64,
    #[serde(default)]
    duration_ms: u64,
    #[serde(default)]
    num_turns: u32,
    #[serde(default)]
    session_id: String,
}

/// Parse the CLI's stdout to find the final result JSON object.
///
/// The output may contain multiple JSON objects (log lines, progress, etc.).
/// We look for the last line containing `"type":"result"`.
fn parse_cli_json(stdout: &str) -> Result<CliResponse> {
    // Try parsing the entire output first (single JSON object).
    if let Ok(resp) = serde_json::from_str::<CliResponse>(stdout.trim()) {
        return Ok(resp);
    }

    // Try each line in reverse, looking for the result object.
    for line in stdout.lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(resp) = serde_json::from_str::<CliResponse>(trimmed) {
            if resp.r#type == "result" || !resp.result.is_empty() {
                return Ok(resp);
            }
        }
    }

    Err(Error::ModelProvider(format!(
        "failed to parse Claude CLI output — no result JSON found. Output: {stdout}"
    )))
}

// ---------------------------------------------------------------------------
// Tool call parsing
// ---------------------------------------------------------------------------

/// Wrapper for the `{"tool_calls": [...]}` format we ask the model to use.
#[derive(Deserialize)]
struct ToolCallResponse {
    tool_calls: Vec<RawToolCall>,
}

#[derive(Deserialize)]
struct RawToolCall {
    name: String,
    arguments: serde_json::Value,
}

/// Try to parse the response text as a tool call JSON object.
///
/// Handles cases where the model wraps the JSON in markdown code fences
/// (despite being told not to).
fn try_parse_tool_calls(text: &str) -> Option<Vec<ToolCall>> {
    // First, try direct parse.
    if let Some(calls) = parse_tool_json(text) {
        return Some(calls);
    }

    // Try stripping markdown code fences: ```json ... ``` or ``` ... ```
    let stripped = text
        .trim()
        .strip_prefix("```json")
        .or_else(|| text.trim().strip_prefix("```"))
        .and_then(|s| s.strip_suffix("```"))
        .map(|s| s.trim());

    if let Some(inner) = stripped {
        if let Some(calls) = parse_tool_json(inner) {
            return Some(calls);
        }
    }

    None
}

fn parse_tool_json(text: &str) -> Option<Vec<ToolCall>> {
    let resp: ToolCallResponse = serde_json::from_str(text).ok()?;
    let calls = resp
        .tool_calls
        .into_iter()
        .map(|tc| ToolCall {
            id: Uuid::new_v4().to_string(),
            name: tc.name,
            arguments: tc.arguments,
        })
        .collect();
    Some(calls)
}
