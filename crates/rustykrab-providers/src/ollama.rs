use async_trait::async_trait;
use chrono::Utc;
use rustykrab_core::error::Result;
use rustykrab_core::model::{ModelProvider, ModelResponse, StopReason, Usage};
use rustykrab_core::types::{
    Message, MessageContent, Role, ToolCall, ToolSchema,
};
use rustykrab_core::Error;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Configuration for Ollama model inference.
#[derive(Debug, Clone)]
pub struct OllamaConfig {
    /// Temperature for sampling (0.0 = deterministic, 0.7 = creative).
    pub temperature: f32,
    /// Context window size in tokens.
    pub num_ctx: u32,
    /// Number of parallel inference slots.
    pub num_parallel: u32,
    /// Top-p nucleus sampling threshold.
    pub top_p: f32,
    /// Number of tokens to predict (-1 = unlimited, 0 = fill context).
    pub num_predict: i32,
}

impl Default for OllamaConfig {
    fn default() -> Self {
        Self {
            temperature: 0.1,
            num_ctx: 131_072,
            num_parallel: 6,
            top_p: 0.9,
            num_predict: 8192,
        }
    }
}

impl OllamaConfig {
    /// Configuration optimized for tool-calling tasks (low temperature).
    pub fn tool_calling() -> Self {
        Self {
            temperature: 0.0,
            num_ctx: 131_072,
            num_parallel: 6,
            top_p: 0.9,
            num_predict: 4096,
        }
    }

    /// Configuration for creative drafting (higher temperature).
    pub fn creative() -> Self {
        Self {
            temperature: 0.7,
            num_ctx: 131_072,
            num_parallel: 6,
            top_p: 0.95,
            num_predict: 16384,
        }
    }
}

/// Ollama provider for local models (Gemma, Qwen, Llama, Mistral, etc.).
pub struct OllamaProvider {
    client: reqwest::Client,
    base_url: String,
    model: String,
    config: OllamaConfig,
}

impl OllamaProvider {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: "http://localhost:11434".to_string(),
            model: model.into(),
            config: OllamaConfig::default(),
        }
    }

    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    pub fn with_config(mut self, config: OllamaConfig) -> Self {
        self.config = config;
        self
    }

    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.config.temperature = temperature;
        self
    }

    /// Get the model name.
    pub fn model(&self) -> &str {
        &self.model
    }

    fn build_messages(messages: &[Message]) -> Vec<OllamaMessage> {
        messages
            .iter()
            .filter_map(|msg| {
                let role = match msg.role {
                    Role::System => "system",
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::Tool => "tool",
                };

                match &msg.content {
                    MessageContent::Text(text) => Some(OllamaMessage {
                        role: role.to_string(),
                        content: Some(text.clone()),
                        tool_calls: None,
                    }),
                    MessageContent::ToolCall(call) => Some(OllamaMessage {
                        role: role.to_string(),
                        content: None,
                        tool_calls: Some(vec![OllamaToolCall {
                            function: OllamaFunction {
                                name: call.name.clone(),
                                arguments: call.arguments.clone(),
                            },
                        }]),
                    }),
                    MessageContent::MultiToolCall(calls) => Some(OllamaMessage {
                        role: role.to_string(),
                        content: None,
                        tool_calls: Some(
                            calls
                                .iter()
                                .map(|c| OllamaToolCall {
                                    function: OllamaFunction {
                                        name: c.name.clone(),
                                        arguments: c.arguments.clone(),
                                    },
                                })
                                .collect(),
                        ),
                    }),
                    MessageContent::ToolResult(result) => Some(OllamaMessage {
                        role: role.to_string(),
                        content: Some(
                            serde_json::to_string(&result.output).unwrap_or_default(),
                        ),
                        tool_calls: None,
                    }),
                }
            })
            .collect()
    }

    fn build_tools(tools: &[ToolSchema]) -> Vec<OllamaTool> {
        tools
            .iter()
            .map(|t| OllamaTool {
                r#type: "function".to_string(),
                function: OllamaToolDef {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.parameters.clone(),
                },
            })
            .collect()
    }

    /// Normalize tool-call arguments: some models (notably Gemma) return
    /// arguments as a JSON-encoded string rather than an object. Detect
    /// that case and parse it into a proper `Value::Object`.
    fn normalize_arguments(args: serde_json::Value) -> serde_json::Value {
        if let serde_json::Value::String(ref s) = args {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(s) {
                if parsed.is_object() {
                    return parsed;
                }
            }
        }
        args
    }

    fn parse_response(resp: OllamaResponse) -> Result<ModelResponse> {
        let msg = resp.message;

        // Collect all tool calls.
        if let Some(tool_calls) = msg.tool_calls {
            if !tool_calls.is_empty() {
                let calls: Vec<ToolCall> = tool_calls
                    .into_iter()
                    .map(|tc| ToolCall {
                        id: Uuid::new_v4().to_string(),
                        name: tc.function.name,
                        arguments: Self::normalize_arguments(tc.function.arguments),
                    })
                    .collect();

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
                    usage: Usage {
                        prompt_tokens: resp.prompt_eval_count.unwrap_or(0),
                        completion_tokens: resp.eval_count.unwrap_or(0),
                    },
                    stop_reason: StopReason::ToolUse,
                });
            }
        }

        Ok(ModelResponse {
            message: Message {
                id: Uuid::new_v4(),
                role: Role::Assistant,
                content: MessageContent::Text(msg.content.unwrap_or_default()),
                created_at: Utc::now(),
            },
            usage: Usage {
                prompt_tokens: resp.prompt_eval_count.unwrap_or(0),
                completion_tokens: resp.eval_count.unwrap_or(0),
            },
            stop_reason: StopReason::EndTurn,
        })
    }
}

#[async_trait]
impl ModelProvider for OllamaProvider {
    fn name(&self) -> &str {
        "ollama"
    }

    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
    ) -> Result<ModelResponse> {
        let ollama_messages = Self::build_messages(messages);
        let ollama_tools = Self::build_tools(tools);

        let mut body = serde_json::json!({
            "model": self.model,
            "messages": ollama_messages,
            "stream": false,
            "options": {
                "temperature": self.config.temperature,
                "num_ctx": self.config.num_ctx,
                "top_p": self.config.top_p,
                "num_predict": self.config.num_predict,
            },
        });

        if !ollama_tools.is_empty() {
            body["tools"] = serde_json::to_value(&ollama_tools)
                .map_err(|e| Error::Serialization(e))?;
        }

        tracing::debug!(model = %self.model, base_url = %self.base_url, "calling Ollama chat API");

        let url = format!("{}/api/chat", self.base_url);
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                Error::ModelProvider(format!(
                    "failed to connect to Ollama at {}: {e}. Is Ollama running?",
                    self.base_url
                ))
            })?;

        let status = resp.status();
        if !status.is_success() {
            let error_body = resp.text().await.unwrap_or_default();
            return Err(Error::ModelProvider(format!(
                "Ollama API returned {status}: {error_body}"
            )));
        }

        let ollama_resp: OllamaResponse = resp
            .json()
            .await
            .map_err(|e| Error::ModelProvider(format!("failed to parse Ollama response: {e}")))?;

        Self::parse_response(ollama_resp)
    }
}

// --- Ollama API wire types (private) ---

#[derive(Serialize)]
struct OllamaMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OllamaToolCall>>,
}

#[derive(Serialize, Deserialize)]
struct OllamaToolCall {
    function: OllamaFunction,
}

#[derive(Serialize, Deserialize)]
struct OllamaFunction {
    name: String,
    arguments: serde_json::Value,
}

#[derive(Serialize)]
struct OllamaTool {
    r#type: String,
    function: OllamaToolDef,
}

#[derive(Serialize)]
struct OllamaToolDef {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Deserialize)]
struct OllamaResponse {
    message: OllamaResponseMessage,
    prompt_eval_count: Option<u32>,
    eval_count: Option<u32>,
}

#[derive(Deserialize)]
struct OllamaResponseMessage {
    content: Option<String>,
    tool_calls: Option<Vec<OllamaToolCall>>,
}
