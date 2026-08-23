use crate::backoff::retry_delay;
use crate::line_buffer::LineBuffer;
use async_trait::async_trait;
use rustykrab_core::error::Result;
use rustykrab_core::model::{ModelProvider, ModelResponse, StopReason, StreamEvent, Usage};
use rustykrab_core::types::{Message, MessageContent, Role, ToolCall, ToolSchema};
use rustykrab_core::Error;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use uuid::Uuid;

/// Maximum number of retries for transient errors (429, 5xx).
const MAX_RETRIES: u32 = 3;
/// Base delay for exponential backoff (doubles each retry, with jitter).
const RETRY_BASE_DELAY: Duration = Duration::from_secs(1);

/// Context window RustyKrab pins by default when the operator hasn't chosen
/// one.
///
/// Pinning matters for KV-cache reuse. Ollama sizes a model runner's KV cache
/// from the `num_ctx` of the request that loads it; a later request asking for
/// a different `num_ctx` forces the scheduler to tear the runner down and
/// reload it, discarding every cached prefix. Omitting `num_ctx` entirely is
/// worse still: the client then has no idea how much context the server
/// actually allocated, so its own trimming budget (below) is a guess, and a
/// prompt that overshoots gets silently truncated server-side — which moves
/// the truncation point every turn and defeats prefix caching completely.
///
/// 64k matches `DEFAULT_COMPACTION_CONTEXT_CEILING` in the agent runner, so
/// the provider's window and the compaction budget agree instead of one
/// silently capping the other. It is clamped down to the model's own native
/// length at startup, so a smaller model still gets a sane value.
///
/// This costs VRAM: the KV cache is allocated for the whole window when the
/// runner loads. Lower it with `RUSTYKRAB_NUM_CTX` if the model won't fit, or
/// halve the cache with `OLLAMA_KV_CACHE_TYPE=q8_0` (see README).
const DEFAULT_NUM_CTX: u32 = 65_536;

/// Default `keep_alive` sent with every request: how long Ollama keeps the
/// model (and its KV cache) resident after the request finishes. Ollama's own
/// default is 5 minutes, which is far too short for a chat gateway that sees
/// sporadic traffic — every message after a quiet spell pays a full model
/// reload plus a cold-cache prompt eval. Override with `OLLAMA_KEEP_ALIVE`.
const DEFAULT_KEEP_ALIVE: &str = "30m";

/// Configuration for Ollama model inference.
#[derive(Debug, Clone)]
pub struct OllamaConfig {
    /// Temperature for sampling (0.0 = deterministic, 0.7 = creative).
    pub temperature: f32,
    /// Explicit context-window size sent to the Ollama server as
    /// `options.num_ctx`. Defaults to [`DEFAULT_NUM_CTX`] so client and
    /// server agree on one stable window; see that constant for why pinning
    /// beats deferring. `None` restores the old behaviour of omitting the
    /// field so the server's `OLLAMA_CONTEXT_LENGTH` (or the per-model
    /// default) wins — select it with `RUSTYKRAB_NUM_CTX=server`.
    pub num_ctx: Option<u32>,
    /// How long Ollama should keep the model resident after a request, in
    /// Ollama's duration syntax (`"30m"`, `"1h"`, `"-1"` for forever).
    /// `None` omits the field and takes the server's 5-minute default.
    pub keep_alive: Option<String>,
    /// Top-p nucleus sampling threshold.
    pub top_p: f32,
    /// Number of tokens to predict (-1 = unlimited, 0 = fill context).
    pub num_predict: i32,
    /// Enable thinking mode. `None` (the default) decides per model via
    /// [`think_support`]; `Some(_)` forces the answer. Ollama rejects
    /// `think` outright for models that don't support it, so this must not
    /// be sent unconditionally.
    pub think: Option<bool>,
}

/// Resolve the `num_ctx` to pin from the environment.
///
/// Checks `RUSTYKRAB_NUM_CTX` first (the canonical RustyKrab-namespaced
/// name), then falls back to `OLLAMA_NUM_CTX`. A numeric value pins that
/// window; `server`/`default`/`0` defers to the Ollama server's own
/// configuration (returning `None`); anything unset falls back to
/// [`DEFAULT_NUM_CTX`].
fn num_ctx_from_env() -> Option<u32> {
    let raw = std::env::var("RUSTYKRAB_NUM_CTX")
        .ok()
        .or_else(|| std::env::var("OLLAMA_NUM_CTX").ok());

    let Some(raw) = raw else {
        return Some(DEFAULT_NUM_CTX);
    };
    let trimmed = raw.trim();

    if matches!(
        trimmed.to_ascii_lowercase().as_str(),
        "server" | "default" | "0" | ""
    ) {
        return None;
    }

    match trimmed.parse::<u32>() {
        Ok(v) => Some(v),
        Err(_) => {
            tracing::warn!(
                value = %raw,
                default_num_ctx = DEFAULT_NUM_CTX,
                "could not parse num_ctx override; falling back to the default"
            );
            Some(DEFAULT_NUM_CTX)
        }
    }
}

/// Resolve `keep_alive` from `OLLAMA_KEEP_ALIVE`. An empty value or the
/// literal `server` omits the field and takes Ollama's own default.
fn keep_alive_from_env() -> Option<String> {
    match std::env::var("OLLAMA_KEEP_ALIVE") {
        Ok(v) => {
            let trimmed = v.trim();
            if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("server") {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        Err(_) => Some(DEFAULT_KEEP_ALIVE.to_string()),
    }
}

/// Rough characters-per-token ratio used for client-side context budgeting.
/// Real tokenization varies by model (English prose ≈ 4, code ≈ 3, CJK ≈ 1-2);
/// 4 is a conservative middle ground that errs toward keeping more history.
const CHARS_PER_TOKEN: usize = 4;

/// Per-message overhead (role tag, framing) the server adds on top of content.
const PER_MESSAGE_OVERHEAD_TOKENS: u32 = 4;

/// Tokens reserved for chat-template framing the client can't measure:
/// role tags, the tool-call preamble most templates emit, BOS/EOS scaffolding.
///
/// Tool schemas used to be lumped in here at a flat 2048. They aren't any
/// more — they're measured per request, because the real figure moves by an
/// order of magnitude as the model loads tools (~1.8k tokens for the set
/// seeded at turn 0, ~10k with the full catalog loaded). A flat guess
/// over-reserved on a fresh conversation and badly under-reserved on a
/// tool-heavy one, which is how a "trimmed" prompt could still overflow.
const FRAMING_OVERHEAD_TOKENS: u32 = 512;

/// Stand-in for the tool-schema block used by [`OllamaProvider::context_limit`],
/// which is called without knowing the conversation's active tool set. Sized
/// for a typical mid-conversation set; the per-request path measures the real
/// thing instead.
const ASSUMED_TOOL_TOKENS: u32 = 2048;

/// Smallest input budget [`OllamaProvider::context_limit`] will report when
/// the reserves exceed the window. Small enough to be honest about a tiny
/// window, large enough that a caller does not compact on every turn.
const MIN_REPORTED_INPUT_BUDGET: u32 = 512;

/// Percentage of the trimming budget to cut down to once trimming fires.
/// See `trim_to_budget` for why this is well below 100.
const TRIM_TARGET_PCT: u32 = 75;

impl Default for OllamaConfig {
    fn default() -> Self {
        Self {
            temperature: 0.1,
            num_ctx: num_ctx_from_env(),
            keep_alive: keep_alive_from_env(),
            top_p: 0.9,
            num_predict: 4096,
            think: None,
        }
    }
}

impl OllamaConfig {
    /// Configuration optimized for tool-calling tasks (low temperature).
    pub fn tool_calling() -> Self {
        Self {
            temperature: 0.0,
            num_predict: 4096,
            ..Self::default()
        }
    }

    /// Configuration for creative drafting (higher temperature).
    ///
    /// Note that `num_predict` is a sampling parameter, so varying it
    /// between presets does not force Ollama to reload the model runner —
    /// unlike `num_ctx`, which is deliberately shared across all presets so
    /// a process that mixes them keeps hitting the same warm KV cache.
    pub fn creative() -> Self {
        Self {
            temperature: 0.7,
            top_p: 0.95,
            num_predict: 16384,
            ..Self::default()
        }
    }
}

/// Ollama provider for local models (Gemma, Qwen, Llama, Mistral, etc.).
pub struct OllamaProvider {
    client: reqwest::Client,
    base_url: String,
    model: String,
    config: OllamaConfig,
    /// Model's native context length discovered from `/api/show`.  Used to
    /// clamp the pinned `num_ctx` down to something the model can actually
    /// serve, and as the client-side prompt-trimming budget when `num_ctx`
    /// has been explicitly set to defer to the server.
    detected_ctx: Option<u32>,
    /// Fingerprint of the tool block sent on the previous request, so a
    /// change can be reported.  See [`OllamaProvider::note_tool_block`].
    /// `0` means "nothing sent yet".
    last_tool_fingerprint: AtomicU64,
}

impl OllamaProvider {
    pub fn new(model: impl Into<String>) -> Self {
        // Large prompts can easily take more than five minutes for prompt
        // evaluation on a local GPU, so allow up to 15 minutes per request
        // before we give up.  Can be overridden via `OLLAMA_TIMEOUT_SECS`.
        let timeout_secs = std::env::var("OLLAMA_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(900);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build HTTP client");
        Self {
            client,
            base_url: "http://localhost:11434".to_string(),
            model: model.into(),
            config: OllamaConfig::default(),
            detected_ctx: None,
            last_tool_fingerprint: AtomicU64::new(0),
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

    /// Get the explicit `num_ctx` that will be sent to the server, if any.
    /// `None` means the server's own configuration is used.
    pub fn num_ctx(&self) -> Option<u32> {
        self.config.num_ctx
    }

    /// Get the `keep_alive` that will be sent with each request, if any.
    pub fn keep_alive(&self) -> Option<&str> {
        self.config.keep_alive.as_deref()
    }

    /// Whether `think` will be sent, and with what value. An explicit
    /// `config.think` wins; otherwise the model tag decides.
    pub fn resolved_think(&self) -> bool {
        self.config
            .think
            .unwrap_or_else(|| think_support(&self.model))
    }

    /// Effective context window used for client-side prompt trimming.
    /// Prefers the user's explicit `num_ctx`, then the value detected from
    /// the model via `/api/show`, else `None` (no trimming).
    pub fn effective_ctx(&self) -> Option<u32> {
        self.config.num_ctx.or(self.detected_ctx)
    }

    /// Query Ollama's `/api/show` endpoint for the loaded model's native
    /// context length.  Returns `Ok(None)` if the response shape is
    /// unfamiliar (e.g. an architecture we don't recognize).  Network and
    /// HTTP errors are propagated so the caller can decide how to react.
    pub async fn detect_context_window(&self) -> Result<Option<u32>> {
        Ok(self.detect_model_shape().await?.0)
    }

    /// Query `/api/show` for both the model's native context length and the
    /// attention geometry needed to size its KV cache.
    async fn detect_model_shape(&self) -> Result<(Option<u32>, Option<KvGeometry>)> {
        let url = format!("{}/api/show", self.base_url);
        let resp = self
            .client
            .post(&url)
            .json(&serde_json::json!({ "model": self.model }))
            .send()
            .await
            .map_err(|e| Error::ModelProvider(format!("failed to query Ollama /api/show: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Self::map_status_error(status, &body));
        }

        let raw: serde_json::Value = resp.json().await.map_err(|e| {
            Error::ModelProvider(format!("failed to parse /api/show response: {e}"))
        })?;
        Ok((
            parse_context_length_from_show(&raw),
            parse_kv_geometry_from_show(&raw),
        ))
    }

    /// Report what the pinned window will cost in KV cache.
    ///
    /// Choosing `num_ctx` is a VRAM decision, and without this the operator
    /// has no way to make it except trial and error against an OOM. Logged at
    /// startup for the window actually in use, plus the model's native
    /// maximum so the gap between "what it supports" and "what it costs" is
    /// visible in one place.
    fn log_kv_cache_estimate(&self, geometry: KvGeometry, native_ctx: Option<u32>) {
        let Some(window) = self.effective_ctx() else {
            return;
        };
        // f16 is Ollama's default; OLLAMA_KV_CACHE_TYPE=q8_0 halves it.
        const F16: u64 = 2;
        let gib = |bytes: u64| bytes as f64 / (1024.0 * 1024.0 * 1024.0);

        tracing::info!(
            num_ctx = window,
            layers = geometry.layers,
            kv_heads = geometry.kv_heads,
            kv_cache_gib_f16 = format!("{:.1}", gib(geometry.cache_bytes(window, F16))),
            kv_cache_gib_q8_0 = format!("{:.1}", gib(geometry.cache_bytes(window, 1))),
            native_ctx_kv_cache_gib_f16 = native_ctx
                .map(|n| format!("{:.1}", gib(geometry.cache_bytes(n, F16))))
                .unwrap_or_else(|| "unknown".to_string()),
            "estimated KV cache footprint (upper bound; sliding-window layers cost less)"
        );
    }

    /// Detect the model's native context length and cache it for client-side
    /// prompt-trimming purposes.  If the user has set an explicit `num_ctx`
    /// that exceeds the detected value, clamp it down so we don't OOM the
    /// server.  On any failure the cached value stays `None` and a warning
    /// is logged — startup must not fail just because Ollama is momentarily
    /// unreachable.
    pub async fn with_detected_context_window(mut self) -> Self {
        let (detected, geometry) = match self.detect_model_shape().await {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!(
                    model = %self.model,
                    num_ctx = ?self.config.num_ctx,
                    error = %e,
                    "failed to query /api/show"
                );
                return self;
            }
        };

        match detected {
            Some(detected) => {
                self.detected_ctx = Some(detected);
                match self.config.num_ctx {
                    Some(requested) if requested > detected => {
                        tracing::info!(
                            model = %self.model,
                            requested_num_ctx = requested,
                            detected_num_ctx = detected,
                            "clamping num_ctx to model's native context length"
                        );
                        self.config.num_ctx = Some(detected);
                    }
                    Some(requested) => {
                        tracing::debug!(
                            model = %self.model,
                            num_ctx = requested,
                            detected_num_ctx = detected,
                            "num_ctx fits within model's native context length"
                        );
                    }
                    None => {
                        tracing::debug!(
                            model = %self.model,
                            detected_num_ctx = detected,
                            "no explicit num_ctx set; deferring to server while using detected value for client-side trimming"
                        );
                    }
                }
            }
            None => {
                tracing::warn!(
                    model = %self.model,
                    num_ctx = ?self.config.num_ctx,
                    "could not detect model context length from /api/show"
                );
            }
        }

        match geometry {
            Some(geometry) => self.log_kv_cache_estimate(geometry, detected),
            None => tracing::debug!(
                model = %self.model,
                "could not read attention geometry from /api/show; skipping KV cache estimate"
            ),
        }
        self
    }

    /// Strip `<think>…</think>` blocks from assistant content so that
    /// model thinking is not re-submitted in conversation history.
    /// Gemma 4 and other thinking models embed reasoning inside these tags;
    /// re-sending them degrades output quality (see model card best practices).
    fn strip_thinking(text: &str) -> String {
        let mut result = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find("<think>") {
            result.push_str(&rest[..start]);
            match rest[start..].find("</think>") {
                Some(end) => {
                    rest = &rest[start + end + "</think>".len()..];
                }
                None => {
                    // Unclosed <think> tag — drop everything from here.
                    rest = "";
                    break;
                }
            }
        }
        result.push_str(rest);
        result
    }

    /// Fix #195: returns Result to propagate serialization errors.
    fn build_messages(messages: &[Message], supports_vision: bool) -> Result<Vec<OllamaMessage>> {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine;
        use rustykrab_core::types::ContentBlock;

        let mut out = Vec::with_capacity(messages.len());
        for msg in messages {
            let role = match msg.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
            };

            match &msg.content {
                MessageContent::Text(text) => {
                    // Strip <think> blocks from assistant messages so
                    // model thinking is not re-submitted in history.
                    let content = if msg.role == Role::Assistant && text.contains("<think>") {
                        let stripped = Self::strip_thinking(text);
                        tracing::debug!(
                            original_len = text.len(),
                            stripped_len = stripped.len(),
                            "stripped thinking blocks from assistant message"
                        );
                        stripped
                    } else {
                        text.clone()
                    };
                    out.push(OllamaMessage {
                        role: role.to_string(),
                        content: Some(content),
                        tool_calls: None,
                        images: None,
                    });
                }
                MessageContent::ToolCall(call) => out.push(OllamaMessage {
                    role: role.to_string(),
                    content: None,
                    tool_calls: Some(vec![OllamaToolCall {
                        function: OllamaFunction {
                            name: call.name.clone(),
                            arguments: call.arguments.clone(),
                        },
                    }]),
                    images: None,
                }),
                MessageContent::MultiToolCall(calls) => out.push(OllamaMessage {
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
                    images: None,
                }),
                MessageContent::ToolResult(result) => {
                    // Fix #182: avoid double-serialization of string values.
                    // Fix #195: propagate serialization errors.
                    let content = match &result.output {
                        serde_json::Value::String(s) => s.clone(),
                        other => serde_json::to_string(other).map_err(Error::Serialization)?,
                    };
                    out.push(OllamaMessage {
                        role: role.to_string(),
                        content: Some(content),
                        tool_calls: None,
                        images: None,
                    });
                    // Ollama's chat API has no image slot on a `tool` message,
                    // so surface any tool-produced images (e.g. screenshots)
                    // as a follow-up user message — but only for vision models.
                    if supports_vision && !result.images.is_empty() {
                        let imgs: Vec<String> = result
                            .images
                            .iter()
                            .filter_map(|b| match b {
                                ContentBlock::Image { data, .. } => Some(STANDARD.encode(data)),
                                _ => None,
                            })
                            .collect();
                        if !imgs.is_empty() {
                            out.push(OllamaMessage {
                                role: "user".to_string(),
                                content: Some(
                                    "Images returned by the previous tool call:".to_string(),
                                ),
                                tool_calls: None,
                                images: Some(imgs),
                            });
                        }
                    }
                }
                MessageContent::MultiPart(blocks) => {
                    let text = blocks
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    // Ollama carries images at the message level as a list of
                    // base64 strings (no data-URI prefix). Callers only place
                    // `Image` blocks here for vision-capable models (gated
                    // upstream by `supports_vision`).
                    let images: Vec<String> = blocks
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Image { data, .. } => Some(STANDARD.encode(data)),
                            _ => None,
                        })
                        .collect();
                    out.push(OllamaMessage {
                        role: role.to_string(),
                        content: Some(text),
                        tool_calls: None,
                        images: (!images.is_empty()).then_some(images),
                    });
                }
            }
        }
        Ok(out)
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

        // Fix #192: parse done_reason to detect truncation.
        let stop_reason = match resp.done_reason.as_deref() {
            Some("length") => StopReason::MaxTokens,
            _ => StopReason::EndTurn, // "stop" or absent → normal end
        };

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
                    message: Message::stamped(Role::Assistant, content),
                    usage: Usage {
                        prompt_tokens: resp.prompt_eval_count.unwrap_or(0),
                        completion_tokens: resp.eval_count.unwrap_or(0),
                        ..Default::default()
                    },
                    stop_reason: StopReason::ToolUse,
                    text: None,
                });
            }
        }

        Ok(ModelResponse {
            message: Message::stamped(
                Role::Assistant,
                MessageContent::Text(msg.content.unwrap_or_default()),
            ),
            usage: Usage {
                prompt_tokens: resp.prompt_eval_count.unwrap_or(0),
                completion_tokens: resp.eval_count.unwrap_or(0),
                ..Default::default()
            },
            stop_reason,
            text: None,
        })
    }

    /// Trim oldest non-system messages until the estimated prompt fits the
    /// budget derived from `total_ctx` minus `num_predict` and a safety margin
    /// for tool schemas / chat-template framing.  Tool-result messages are
    /// dropped along with any preceding orphaned tool-call assistant turn so
    /// the request stays well-formed.  When `total_ctx` is `None` we have no
    /// budget to enforce so messages pass through unchanged.
    ///
    /// Trimming is deliberately hysteretic: once it fires it drops down to
    /// [`TRIM_TARGET_PCT`] of the budget rather than to the first arrangement
    /// that fits.  Dropping the oldest messages rewrites the prompt directly
    /// after the system block, which invalidates every cached token past that
    /// point.  Trimming to exactly-fits means the very next turn overflows
    /// again, so *every* subsequent request re-evaluates the whole prompt from
    /// scratch.  Cutting deeper, less often, confines that cost to one turn in
    /// many.
    fn trim_to_budget(
        messages: Vec<OllamaMessage>,
        total_ctx: Option<u32>,
        num_predict: i32,
        tool_tokens: u32,
    ) -> Vec<OllamaMessage> {
        let Some(total_ctx) = total_ctx else {
            return messages;
        };
        let budget = input_budget(total_ctx, num_predict, tool_tokens);

        let total: u32 = messages.iter().map(estimate_message_tokens).sum();
        if total <= budget {
            return messages;
        }

        // Target for this trim. `budget` remains the trigger; this is how far
        // below it we cut once triggered.
        let target = (budget as u64 * TRIM_TARGET_PCT as u64 / 100) as u32;

        let system_count = messages.iter().take_while(|m| m.role == "system").count();
        let mut trimmed = messages;
        let mut current = total;

        // Walk forward from the first non-system message counting how many
        // to drop, then remove them with a single `drain` — per-message
        // `Vec::remove` would shift the entire tail once per drop (O(n·k)).
        //
        // The final message is always the turn we are actually asking about,
        // so stop one short of the end: cutting to `target` must never eat
        // the live request.
        let last = trimmed.len().saturating_sub(1);
        let mut drop_end = system_count;
        while current > target && drop_end < last {
            current = current.saturating_sub(estimate_message_tokens(&trimmed[drop_end]));
            drop_end += 1;
        }

        // If trimming left a leading orphan tool-result (no preceding
        // assistant tool_call), drop it so Ollama doesn't reject the request.
        while drop_end < trimmed.len() && trimmed[drop_end].role == "tool" {
            drop_end += 1;
        }

        // Never trim past the most recent user turn. A request whose message
        // array carries no `user` role is rejected outright by Ollama with
        // `no user query found in messages`, turning an over-long conversation
        // into a hard failure instead of a merely degraded one.
        //
        // The `drop_end < last` bound above already spares the final message,
        // but that is not the same guarantee: the final message is only a user
        // turn on some paths (a scheduled run whose conversation ends in an
        // assistant or tool turn is the motivating case), and the orphan-tool
        // loop is bounded by `trimmed.len()` rather than `last`, so it can walk
        // past the end and drain everything but the system prompt. Clamp
        // explicitly.
        //
        // Clamping can leave the request above budget; that is strictly
        // preferable to a guaranteed provider rejection.
        if let Some(idx) = trimmed.iter().rposition(|m| m.role == "user") {
            drop_end = drop_end.min(idx);
        }

        let dropped = drop_end - system_count;
        trimmed.drain(system_count..drop_end);

        // `current` was accumulated by the drop loops and goes stale whenever
        // the clamp above spared messages they had already counted. Recompute
        // from what actually survived so the log line is truthful.
        let remaining: u32 = trimmed.iter().map(estimate_message_tokens).sum();

        tracing::warn!(
            num_ctx = total_ctx,
            budget,
            target,
            estimated_tokens_before = total,
            estimated_tokens_after = remaining,
            messages_dropped = dropped,
            "trimmed conversation history to fit Ollama context window"
        );

        // Preserving the last user turn can leave us over budget. That means a
        // single turn is too large for the context window, which needs its own
        // handling rather than silently shipping an over-budget request.
        if remaining > budget {
            tracing::error!(
                num_ctx = total_ctx,
                budget,
                estimated_tokens_after = remaining,
                "conversation still exceeds the Ollama input budget after trimming; \
                 a single turn is larger than the context window"
            );
        }

        trimmed
    }

    /// Record the tool block about to be sent, and report when it differs
    /// from the previous request's.
    ///
    /// This is not bookkeeping for its own sake. Chat templates render tool
    /// definitions into the prompt *prefix*, ahead of the conversation, so
    /// changing the tool set moves every subsequent token — the cached prefix
    /// stops matching at the tool block and the server re-evaluates the whole
    /// prompt. On a long conversation that is by far the most expensive thing
    /// that can happen to a turn, and it is invisible without this log line.
    ///
    /// The set is driven by the `tools_load` meta-tool, so it changes when the
    /// model discovers and loads new tools. That is the intended design — the
    /// full catalog is far too large to send every turn — but it means tool
    /// loading is best done in one batch early, not drip-fed across a run.
    fn note_tool_block(&self, tools: &[OllamaTool], tool_tokens: u32) {
        // Names alone, in order: that is what the prompt prefix is sensitive
        // to, and it avoids re-hashing the (much larger) parameter schemas.
        let mut hasher = DefaultHasher::new();
        for t in tools {
            t.function.name.hash(&mut hasher);
        }
        // Reserve 0 for "nothing sent yet" so the first request isn't
        // mistaken for a change.
        let fingerprint = hasher.finish() | 1;

        let previous = self
            .last_tool_fingerprint
            .swap(fingerprint, Ordering::Relaxed);
        if previous != 0 && previous != fingerprint {
            tracing::info!(
                num_tools = tools.len(),
                tool_tokens,
                "tool set changed since the last request — Ollama must re-evaluate                  the whole prompt, since tool definitions sit in the cached prefix"
            );
        }

        // Compaction is supposed to run before trimming: it summarizes and
        // archives, where trimming just drops the oldest turns. The runner
        // derives its threshold from `context_limit()`, which has to assume a
        // typical tool block; if the real one is much bigger, the trimming
        // budget falls below that threshold and trimming pre-empts compaction.
        if let Some(window) = self.effective_ctx() {
            let assumed = input_budget(window, self.config.num_predict, ASSUMED_TOOL_TOKENS);
            let actual = input_budget(window, self.config.num_predict, tool_tokens);
            // The runner compacts at 85% of the budget it was told about.
            let compaction_threshold = assumed / 100 * 85;
            if actual < compaction_threshold {
                tracing::warn!(
                    num_ctx = window,
                    tool_tokens,
                    trim_budget = actual,
                    compaction_threshold,
                    "loaded tool schemas are large enough that history trimming will                      pre-empt compaction — raise RUSTYKRAB_NUM_CTX or load fewer tools"
                );
            }
        }
    }

    /// Map an HTTP status code to a specific error variant (#186).
    fn map_status_error(status: reqwest::StatusCode, body: &str) -> Error {
        match status.as_u16() {
            400 => Error::ModelBadRequest(format!("Ollama API: {body}")),
            401 | 403 => Error::ModelAuthError(format!("Ollama API: {body}")),
            429 => Error::ModelRateLimit(format!("Ollama API: {body}")),
            _ => Error::ModelProvider(format!("Ollama API returned {status}: {body}")),
        }
    }
}

#[async_trait]
impl ModelProvider for OllamaProvider {
    fn name(&self) -> &str {
        "ollama"
    }

    /// Reports the usable *input* budget rather than the raw window.
    ///
    /// The trait documents this as the single source of truth for downstream
    /// budgets — compaction thresholds, prompt trimming — and those budgets
    /// are about how much history fits, not how big the window is. Reporting
    /// the raw window put the runner's compaction threshold (85% of the
    /// window) *above* this provider's trimming budget (window minus output
    /// and overhead), so trimming always fired first and compaction was
    /// effectively unreachable on Ollama. Subtracting the same reservations
    /// here restores the intended order: compact first, trim only as a
    /// backstop.
    fn context_limit(&self) -> Option<usize> {
        self.effective_ctx().map(|window| {
            let budget = input_budget(window, self.config.num_predict, ASSUMED_TOOL_TOKENS);
            if budget > 0 {
                return budget as usize;
            }
            // The reserves swallowed the whole window. Reporting `None`
            // here reads as "I don't know my limit", and the caller then
            // falls back to the profile's `max_context_tokens` — a number
            // this provider will never honour, because the per-request
            // path still trims against the real budget. The visible
            // result is that compaction never fires and history is
            // silently trimmed away instead of being summarised.
            //
            // Report a floor instead, so the caller compacts at a point
            // that is actually reachable, and say once why.
            let floor = (window / 4).max(MIN_REPORTED_INPUT_BUDGET);
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                tracing::warn!(
                    num_ctx = window,
                    num_predict = self.config.num_predict,
                    assumed_tool_tokens = ASSUMED_TOOL_TOKENS,
                    framing_overhead = FRAMING_OVERHEAD_TOKENS,
                    reported_budget = floor,
                    "num_predict and the fixed reserves exceed num_ctx, leaving no room for \
                     input; reporting a floor so compaction still engages. Raise \
                     RUSTYKRAB_NUM_CTX or lower num_predict."
                );
            });
            floor as usize
        })
    }

    fn supports_vision(&self) -> bool {
        vision_support(&self.model)
    }

    async fn chat(&self, messages: &[Message], tools: &[ToolSchema]) -> Result<ModelResponse> {
        let ollama_messages = Self::build_messages(messages, self.supports_vision())?;

        // Fix #200: validate non-empty messages.
        if ollama_messages.is_empty() {
            return Err(Error::ModelBadRequest(
                "cannot call Ollama API with an empty message list".into(),
            ));
        }

        let ollama_tools = Self::build_tools(tools);
        let tool_tokens = estimate_tool_tokens(&ollama_tools);
        self.note_tool_block(&ollama_tools, tool_tokens);

        let ollama_messages = Self::trim_to_budget(
            ollama_messages,
            self.effective_ctx(),
            self.config.num_predict,
            tool_tokens,
        );

        let mut options = serde_json::json!({
            "temperature": self.config.temperature,
            "top_p": self.config.top_p,
            "num_predict": self.config.num_predict,
        });
        // Only override the server's context length when the user has asked
        // for it explicitly — otherwise leave `num_ctx` out so `OLLAMA_CONTEXT_LENGTH`
        // (or the model default) wins.
        if let Some(num_ctx) = self.config.num_ctx {
            options["num_ctx"] = serde_json::json!(num_ctx);
        }

        let mut body = serde_json::json!({
            "model": self.model,
            "messages": ollama_messages,
            "stream": false,
            "options": options,
        });
        // `think` is rejected outright by models that don't support it, so
        // it is only sent when the model (or an explicit override) says yes.
        if self.resolved_think() {
            body["think"] = serde_json::json!(true);
        }
        // Keep the model — and with it the KV cache built from this prompt —
        // resident between turns. Without this Ollama evicts after five idle
        // minutes and the next message pays a reload plus a cold prompt eval.
        if let Some(keep_alive) = &self.config.keep_alive {
            body["keep_alive"] = serde_json::json!(keep_alive);
        }

        if !ollama_tools.is_empty() {
            body["tools"] = serde_json::to_value(&ollama_tools).map_err(Error::Serialization)?;
        }

        tracing::debug!(
            model = %self.model,
            base_url = %self.base_url,
            num_messages = ollama_messages.len(),
            num_ctx = ?self.config.num_ctx,
            num_tools = ollama_tools.len(),
            tool_tokens,
            trace_id = ?rustykrab_core::prompt_trace::current_trace_id(),
            "calling Ollama chat API"
        );
        rustykrab_core::prompt_trace::record_prompt(
            self.name(),
            &self.model,
            false,
            messages,
            tools,
        );

        let url = format!("{}/api/chat", self.base_url);

        let request_start = std::time::Instant::now();
        let mut last_err = None;
        for attempt in 0..=MAX_RETRIES {
            if attempt > 0 {
                let delay = retry_delay(RETRY_BASE_DELAY, attempt);
                tracing::warn!(attempt, "retrying Ollama API after {delay:?}");
                tokio::time::sleep(delay).await;
            }

            let resp = match self.client.post(&url).json(&body).send().await {
                Ok(r) => r,
                Err(e) => {
                    // Don't retry timeouts: retrying the same 99K-token prompt
                    // would just burn another 15-minute budget for the same
                    // failure.  The caller (agent loop) needs to reduce context
                    // or abort before re-trying.
                    if e.is_timeout() {
                        tracing::warn!(
                            model = %self.model,
                            num_messages = ollama_messages.len(),
                            num_ctx = ?self.config.num_ctx,
                            "Ollama request timed out — not retrying (reduce context or raise OLLAMA_TIMEOUT_SECS)"
                        );
                        return Err(Error::ModelProvider(format!(
                            "Ollama request timed out after the configured HTTP timeout. \
                             Reduce prompt size or raise OLLAMA_TIMEOUT_SECS: {e}"
                        )));
                    }
                    last_err = Some(Error::ModelProvider(format!(
                        "failed to connect to Ollama at {}: {e}. Is Ollama running?",
                        self.base_url
                    )));
                    continue;
                }
            };

            let status = resp.status();
            if status.is_success() {
                let raw_body = resp.text().await.map_err(|e| {
                    Error::ModelProvider(format!("failed to read Ollama response body: {e}"))
                })?;
                let ollama_resp: OllamaResponse = serde_json::from_str(&raw_body).map_err(|e| {
                    Error::ModelProvider(format!("failed to parse Ollama response: {e}"))
                })?;
                let response = Self::parse_response(ollama_resp)?;

                // Debug: dump raw response when message text is empty
                // despite having completion tokens.
                if response.usage.completion_tokens > 0
                    && !response.message.content.has_tool_calls()
                    && response
                        .message
                        .content
                        .as_text()
                        .is_none_or(|t| t.is_empty())
                {
                    tracing::warn!(
                        completion_tokens = response.usage.completion_tokens,
                        ?response.stop_reason,
                        "empty message text with completion tokens — dumping raw response"
                    );
                    tracing::warn!(raw_body = %raw_body, "raw Ollama API response");
                }

                rustykrab_core::prompt_trace::record_response(
                    self.name(),
                    &self.model,
                    false,
                    &response.message,
                    &response.usage,
                    &response.stop_reason,
                    request_start.elapsed().as_millis() as u64,
                );
                return Ok(response);
            }

            let error_body = resp.text().await.unwrap_or_default();
            tracing::warn!(
                status = %status,
                num_ctx = ?self.config.num_ctx,
                num_messages = ollama_messages.len(),
                error_body = %error_body,
                "Ollama API error"
            );
            let is_retryable = matches!(status.as_u16(), 429 | 500 | 502 | 503 | 529);
            // Fix #186: map status codes to specific error variants.
            last_err = Some(Self::map_status_error(status, &error_body));

            if !is_retryable {
                break;
            }
        }

        Err(last_err.unwrap_or_else(|| Error::ModelProvider("request failed".into())))
    }

    /// Fix #175: streaming implementation using Ollama NDJSON.
    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        on_event: &(dyn Fn(StreamEvent) + Send + Sync),
    ) -> Result<ModelResponse> {
        let ollama_messages = Self::build_messages(messages, self.supports_vision())?;

        if ollama_messages.is_empty() {
            return Err(Error::ModelBadRequest(
                "cannot call Ollama API with an empty message list".into(),
            ));
        }

        let ollama_tools = Self::build_tools(tools);
        let tool_tokens = estimate_tool_tokens(&ollama_tools);
        self.note_tool_block(&ollama_tools, tool_tokens);

        let ollama_messages = Self::trim_to_budget(
            ollama_messages,
            self.effective_ctx(),
            self.config.num_predict,
            tool_tokens,
        );

        let mut options = serde_json::json!({
            "temperature": self.config.temperature,
            "top_p": self.config.top_p,
            "num_predict": self.config.num_predict,
        });
        if let Some(num_ctx) = self.config.num_ctx {
            options["num_ctx"] = serde_json::json!(num_ctx);
        }

        let mut body = serde_json::json!({
            "model": self.model,
            "messages": ollama_messages,
            "stream": true,
            "options": options,
        });
        // `think` is rejected outright by models that don't support it, so
        // it is only sent when the model (or an explicit override) says yes.
        if self.resolved_think() {
            body["think"] = serde_json::json!(true);
        }
        // Keep the model — and with it the KV cache built from this prompt —
        // resident between turns. Without this Ollama evicts after five idle
        // minutes and the next message pays a reload plus a cold prompt eval.
        if let Some(keep_alive) = &self.config.keep_alive {
            body["keep_alive"] = serde_json::json!(keep_alive);
        }

        if !ollama_tools.is_empty() {
            body["tools"] = serde_json::to_value(&ollama_tools).map_err(Error::Serialization)?;
        }

        tracing::debug!(
            model = %self.model,
            base_url = %self.base_url,
            num_messages = ollama_messages.len(),
            num_ctx = ?self.config.num_ctx,
            num_tools = ollama_tools.len(),
            tool_tokens,
            trace_id = ?rustykrab_core::prompt_trace::current_trace_id(),
            "calling Ollama chat API (streaming)"
        );
        rustykrab_core::prompt_trace::record_prompt(
            self.name(),
            &self.model,
            true,
            messages,
            tools,
        );

        let url = format!("{}/api/chat", self.base_url);

        // Retry the initial connection with the same backoff as the non-streaming path.
        let mut last_err = None;
        let mut resp = None;
        for attempt in 0..=MAX_RETRIES {
            if attempt > 0 {
                let delay = retry_delay(RETRY_BASE_DELAY, attempt);
                tracing::warn!(attempt, "retrying Ollama streaming API after {delay:?}");
                tokio::time::sleep(delay).await;
            }

            let r = match self.client.post(&url).json(&body).send().await {
                Ok(r) => r,
                Err(e) => {
                    // Don't retry timeouts — see `chat` for rationale.
                    if e.is_timeout() {
                        tracing::warn!(
                            model = %self.model,
                            num_messages = ollama_messages.len(),
                            num_ctx = ?self.config.num_ctx,
                            "Ollama streaming request timed out — not retrying"
                        );
                        return Err(Error::ModelProvider(format!(
                            "Ollama streaming request timed out after the configured HTTP timeout. \
                             Reduce prompt size or raise OLLAMA_TIMEOUT_SECS: {e}"
                        )));
                    }
                    last_err = Some(Error::ModelProvider(format!(
                        "failed to connect to Ollama at {}: {e}. Is Ollama running?",
                        self.base_url
                    )));
                    continue;
                }
            };

            let status = r.status();
            if !status.is_success() {
                let error_body = r.text().await.unwrap_or_default();
                tracing::warn!(
                    status = %status,
                    num_ctx = ?self.config.num_ctx,
                    num_messages = ollama_messages.len(),
                    error_body = %error_body,
                    "Ollama streaming API error"
                );
                let is_retryable = matches!(status.as_u16(), 429 | 500 | 502 | 503 | 529);
                last_err = Some(Self::map_status_error(status, &error_body));
                if !is_retryable {
                    break;
                }
                continue;
            }

            resp = Some(r);
            break;
        }
        let resp = resp.ok_or_else(|| {
            last_err.unwrap_or_else(|| Error::ModelProvider("request failed".into()))
        })?;

        // Parse newline-delimited JSON chunks. Raw bytes are buffered and
        // split on `\n` before UTF-8 decoding, so multi-byte codepoints
        // that span network chunks are reassembled instead of corrupted.
        let mut buffer = LineBuffer::new();
        let mut full_text = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut prompt_eval_count: u32 = 0;
        let mut eval_count: u32 = 0;
        let mut done_reason: Option<String> = None;

        let mut response = resp;
        let mut chunks_received: u64 = 0;
        let mut bytes_received: u64 = 0;
        let stream_start = std::time::Instant::now();
        loop {
            let chunk = match response.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break, // stream finished normally
                Err(e) => {
                    let elapsed = stream_start.elapsed();
                    tracing::warn!(
                        error = %e,
                        error_debug = ?e,
                        model = %self.model,
                        base_url = %self.base_url,
                        chunks_received,
                        bytes_received,
                        elapsed_ms = elapsed.as_millis() as u64,
                        accumulated_text_len = full_text.len(),
                        buffer_len = buffer.len(),
                        "stream read error mid-stream, returning partial response"
                    );
                    break;
                }
            };
            chunks_received += 1;
            bytes_received += chunk.len() as u64;
            buffer.push_chunk(&chunk);

            while let Some(line) = buffer.next_line() {
                let line = line.trim();

                if line.is_empty() {
                    continue;
                }

                let stream_chunk: OllamaStreamChunk = serde_json::from_str(line).map_err(|e| {
                    Error::ModelProvider(format!("failed to parse Ollama stream chunk: {e}"))
                })?;

                if let Some(ref content) = stream_chunk.message.content {
                    if !content.is_empty() {
                        full_text.push_str(content);
                        on_event(StreamEvent::TextDelta(content.clone()));
                    }
                }

                // Collect tool calls from the final chunk.
                if let Some(tcs) = stream_chunk.message.tool_calls {
                    for tc in tcs {
                        tool_calls.push(ToolCall {
                            id: Uuid::new_v4().to_string(),
                            name: tc.function.name,
                            arguments: Self::normalize_arguments(tc.function.arguments),
                        });
                    }
                }

                if stream_chunk.done {
                    prompt_eval_count = stream_chunk.prompt_eval_count.unwrap_or(0);
                    eval_count = stream_chunk.eval_count.unwrap_or(0);
                    done_reason = stream_chunk.done_reason;
                }
            }
        }

        let stream_elapsed = stream_start.elapsed();
        tracing::debug!(
            chunks_received,
            bytes_received,
            elapsed_ms = stream_elapsed.as_millis() as u64,
            text_len = full_text.len(),
            "Ollama stream completed"
        );

        let stop_reason = if !tool_calls.is_empty() {
            StopReason::ToolUse
        } else {
            match done_reason.as_deref() {
                Some("length") => StopReason::MaxTokens,
                _ => StopReason::EndTurn,
            }
        };

        let content = if !tool_calls.is_empty() {
            if tool_calls.len() == 1 {
                MessageContent::ToolCall(tool_calls.into_iter().next().unwrap())
            } else {
                MessageContent::MultiToolCall(tool_calls)
            }
        } else {
            MessageContent::Text(full_text)
        };

        let response = ModelResponse {
            message: Message::stamped(Role::Assistant, content),
            usage: Usage {
                prompt_tokens: prompt_eval_count,
                completion_tokens: eval_count,
                ..Default::default()
            },
            stop_reason,
            text: None,
        };

        // Debug: dump response info when message text is empty
        // despite having completion tokens.
        if response.usage.completion_tokens > 0
            && !response.message.content.has_tool_calls()
            && response
                .message
                .content
                .as_text()
                .is_none_or(|t| t.is_empty())
        {
            tracing::warn!(
                completion_tokens = response.usage.completion_tokens,
                ?response.stop_reason,
                "streaming: empty message text with completion tokens"
            );
        }

        rustykrab_core::prompt_trace::record_response(
            self.name(),
            &self.model,
            true,
            &response.message,
            &response.usage,
            &response.stop_reason,
            stream_start.elapsed().as_millis() as u64,
        );
        on_event(StreamEvent::Done(response.clone()));
        Ok(response)
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
    /// Base64-encoded images attached to this message (no data-URI prefix).
    /// Ollama's chat API takes images at the message level rather than as
    /// individual content blocks. Only populated for vision-capable models.
    #[serde(skip_serializing_if = "Option::is_none")]
    images: Option<Vec<String>>,
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
    /// Fix #192: parse done_reason to detect truncation.
    #[serde(default)]
    done_reason: Option<String>,
}

#[derive(Deserialize)]
struct OllamaResponseMessage {
    content: Option<String>,
    tool_calls: Option<Vec<OllamaToolCall>>,
}

/// Streaming chunk from Ollama's NDJSON response.
#[derive(Deserialize)]
struct OllamaStreamChunk {
    message: OllamaStreamMessage,
    done: bool,
    #[serde(default)]
    done_reason: Option<String>,
    #[serde(default)]
    prompt_eval_count: Option<u32>,
    #[serde(default)]
    eval_count: Option<u32>,
}

#[derive(Deserialize)]
struct OllamaStreamMessage {
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OllamaToolCall>>,
}

/// Usable input budget: what's left of the context window once the output
/// reservation, the tool-schema block, and chat-template framing are taken
/// out. This is the single figure both the client-side trimmer and
/// [`OllamaProvider::context_limit`] derive from, so the agent runner's
/// compaction threshold and this provider's trimming budget can't drift into
/// disagreeing about how much room there is.
fn input_budget(window: u32, num_predict: i32, tool_tokens: u32) -> u32 {
    window
        .saturating_sub(num_predict.max(0) as u32)
        .saturating_sub(tool_tokens)
        .saturating_sub(FRAMING_OVERHEAD_TOKENS)
}

/// Measure the tool-schema block exactly as it will be serialized into the
/// request body. Tool definitions are rendered into the prompt *prefix* by
/// essentially every chat template Ollama ships, so this is both a real cost
/// against the window and — when the set changes mid-conversation — the point
/// at which the cached prefix stops matching.
fn estimate_tool_tokens(tools: &[OllamaTool]) -> u32 {
    let mut w = CountingWriter(0);
    // Serializing into an infallible sink cannot fail.
    let _ = serde_json::to_writer(&mut w, tools);
    w.0.div_ceil(CHARS_PER_TOKEN) as u32
}

/// Approximate the number of prompt tokens an `OllamaMessage` will cost.
/// Errs on the high side so trimming converges instead of oscillating.
fn estimate_message_tokens(msg: &OllamaMessage) -> u32 {
    let mut tokens = PER_MESSAGE_OVERHEAD_TOKENS;
    if let Some(c) = &msg.content {
        tokens = tokens.saturating_add(estimate_text_tokens(c));
    }
    if let Some(tcs) = &msg.tool_calls {
        for tc in tcs {
            tokens = tokens.saturating_add(8);
            tokens = tokens.saturating_add(estimate_text_tokens(&tc.function.name));
            tokens = tokens.saturating_add(estimate_json_tokens(&tc.function.arguments));
        }
    }
    tokens
}

/// `io::Write` sink that counts bytes without storing them, so a JSON
/// value's serialized size can be measured without allocating the string.
struct CountingWriter(usize);

impl std::io::Write for CountingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Estimate tokens for a JSON value from its serialized byte length.
/// Bytes ≥ chars, so multibyte content is over-counted slightly — this
/// errs on the high side, consistent with the trimming policy above.
fn estimate_json_tokens(v: &serde_json::Value) -> u32 {
    let mut w = CountingWriter(0);
    // Serializing a `Value` into an infallible sink cannot fail.
    let _ = serde_json::to_writer(&mut w, v);
    w.0.div_ceil(CHARS_PER_TOKEN) as u32
}

fn estimate_text_tokens(s: &str) -> u32 {
    // chars().count() (not len()) so multibyte characters aren't over-counted.
    let chars = s.chars().count();
    chars.div_ceil(CHARS_PER_TOKEN) as u32
}

/// Attention geometry needed to size a model's KV cache, read from
/// `/api/show`'s `model_info` (which surfaces the GGUF metadata).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KvGeometry {
    /// Number of transformer blocks (layers).
    pub layers: u32,
    /// Key/value heads per layer. Smaller than the query head count on any
    /// model using grouped-query attention, which is what makes long
    /// contexts affordable at all.
    pub kv_heads: u32,
    /// Per-head key dimension.
    pub key_length: u32,
    /// Per-head value dimension.
    pub value_length: u32,
}

impl KvGeometry {
    /// Bytes of KV cache a context of `num_ctx` tokens needs, at
    /// `bytes_per_element` per stored scalar (2 for the f16 default, 1 for
    /// `OLLAMA_KV_CACHE_TYPE=q8_0`).
    ///
    /// This is an **upper bound**. Models that interleave sliding-window
    /// local attention with global attention — Gemma's architecture does
    /// exactly this — only allocate the full window for the global layers,
    /// so their real footprint is a fraction of this figure. Treat it as
    /// "no more than", not "exactly".
    pub fn cache_bytes(&self, num_ctx: u32, bytes_per_element: u64) -> u64 {
        let per_token = self.layers as u64
            * self.kv_heads as u64
            * (self.key_length as u64 + self.value_length as u64)
            * bytes_per_element;
        per_token * num_ctx as u64
    }
}

/// Read a `model_info` field that may be stored as a scalar or as a
/// per-layer array. Arrays take the maximum, so the estimate stays an
/// upper bound for models with heterogeneous layers.
fn model_info_u32(info: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<u32> {
    let v = info.get(key)?;
    let n = match v {
        serde_json::Value::Array(items) => items.iter().filter_map(|i| i.as_u64()).max()?,
        other => other.as_u64()?,
    };
    u32::try_from(n).ok().filter(|&n| n > 0)
}

/// Pull the attention geometry out of a `/api/show` response, so the KV
/// cache cost of a given window can be reported rather than guessed at.
/// Returns `None` when the metadata doesn't carry enough to compute it.
fn parse_kv_geometry_from_show(raw: &serde_json::Value) -> Option<KvGeometry> {
    let info = raw.get("model_info")?.as_object()?;
    let arch = info.get("general.architecture")?.as_str()?;

    let layers = model_info_u32(info, &format!("{arch}.block_count"))?;
    let kv_heads = model_info_u32(info, &format!("{arch}.attention.head_count_kv"))?;

    // `key_length`/`value_length` are optional in GGUF; when absent the head
    // dimension is embedding_length / head_count.
    let fallback_head_dim = || {
        let embedding = model_info_u32(info, &format!("{arch}.embedding_length"))?;
        let heads = model_info_u32(info, &format!("{arch}.attention.head_count"))?;
        Some(embedding / heads).filter(|&d| d > 0)
    };
    let key_length =
        model_info_u32(info, &format!("{arch}.attention.key_length")).or_else(fallback_head_dim)?;
    let value_length = model_info_u32(info, &format!("{arch}.attention.value_length"))
        .or_else(fallback_head_dim)?;

    Some(KvGeometry {
        layers,
        kv_heads,
        key_length,
        value_length,
    })
}

/// Pull a context-length value out of a `/api/show` response.  Ollama reports
/// it under `model_info` keyed by architecture (e.g. `llama.context_length`,
/// `qwen3.context_length`).  We accept any key suffixed `.context_length`.
fn parse_context_length_from_show(raw: &serde_json::Value) -> Option<u32> {
    let info = raw.get("model_info")?.as_object()?;

    let arch = raw
        .get("model_info")
        .and_then(|m| m.get("general.architecture"))
        .and_then(|v| v.as_str());

    if let Some(arch) = arch {
        let key = format!("{arch}.context_length");
        if let Some(v) = info.get(&key).and_then(|v| v.as_u64()) {
            return Some(v.min(u32::MAX as u64) as u32);
        }
    }

    // Fallback: any `*.context_length` field.
    for (k, v) in info {
        if k.ends_with(".context_length") {
            if let Some(n) = v.as_u64() {
                return Some(n.min(u32::MAX as u64) as u32);
            }
        }
    }
    None
}

/// Decide whether the configured Ollama model can accept image input.
///
/// Vision is a per-model property in Ollama (the same provider serves both
/// text-only and multimodal models), so we key off the model tag. The
/// `OLLAMA_VISION` env var overrides the heuristic: `true`/`1`/`on` force it
/// on, `false`/`0`/`off` force it off, and `auto` (the default) falls back to
/// `model_supports_vision`.
fn vision_support(model: &str) -> bool {
    match std::env::var("OLLAMA_VISION").ok().as_deref() {
        Some(v) => match v.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "on" | "yes" => true,
            "false" | "0" | "off" | "no" => false,
            // "auto" or anything unrecognized: fall through to the heuristic.
            _ => model_supports_vision(model),
        },
        None => model_supports_vision(model),
    }
}

/// Decide whether to ask the configured Ollama model to think.
///
/// Ollama returns a 400 for `think: true` against a model that has no
/// thinking capability, so this cannot be sent unconditionally. The
/// `OLLAMA_THINK` env var overrides the heuristic with the same
/// `true`/`false`/`auto` vocabulary as `OLLAMA_VISION`.
fn think_support(model: &str) -> bool {
    match std::env::var("OLLAMA_THINK").ok().as_deref() {
        Some(v) => match v.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "on" | "yes" => true,
            "false" | "0" | "off" | "no" => false,
            // "auto" or anything unrecognized: fall through to the heuristic.
            _ => model_supports_thinking(model),
        },
        None => model_supports_thinking(model),
    }
}

/// Heuristic match against known thinking-capable Ollama model families.
///
/// Thinking costs output tokens on every turn and its reasoning is stripped
/// from history before the next call, so it is worth enabling only where it
/// measurably helps tool-call accuracy. Users can force the answer either
/// way with `OLLAMA_THINK`.
fn model_supports_thinking(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    const THINKING_FAMILIES: &[&str] = &[
        "gemma4",
        "deepseek-r1",
        "deepseek-v3.1",
        "qwen3",
        "qwq",
        "gpt-oss",
        "magistral",
        "cogito",
        "smallthinker",
    ];
    THINKING_FAMILIES.iter().any(|fam| m.contains(fam))
}

/// Heuristic match against known vision-capable Ollama model families.
///
/// Matches on the model tag (e.g. `gemma4:26b`, `llava:13b`). New multimodal
/// families can be added here; when in doubt users can force the answer with
/// `OLLAMA_VISION`.
fn model_supports_vision(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    const VISION_FAMILIES: &[&str] = &[
        "gemma3",
        "gemma4",
        "llava",
        "bakllava",
        "llama3.2-vision",
        "llama4",
        "qwen2-vl",
        "qwen2.5vl",
        "qwen2.5-vl",
        "qwen3-vl",
        "minicpm-v",
        "moondream",
        "granite3.2-vision",
        "mistral-small3.1",
        "mistral-small3.2",
        "pixtral",
    ];
    VISION_FAMILIES.iter().any(|fam| m.contains(fam))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    /// Tool-block size used by the trimming tests. Chosen so that
    /// `TEST_TOOL_TOKENS + FRAMING_OVERHEAD_TOKENS` equals the flat 2048 these
    /// cases were originally written against, keeping their arithmetic intact.
    const TEST_TOOL_TOKENS: u32 = 1536;

    fn user_msg(content: &str) -> OllamaMessage {
        OllamaMessage {
            role: "user".to_string(),
            content: Some(content.to_string()),
            tool_calls: None,
            images: None,
        }
    }

    fn system_msg(content: &str) -> OllamaMessage {
        OllamaMessage {
            role: "system".to_string(),
            content: Some(content.to_string()),
            tool_calls: None,
            images: None,
        }
    }

    fn tool_msg(content: &str) -> OllamaMessage {
        OllamaMessage {
            role: "tool".to_string(),
            content: Some(content.to_string()),
            tool_calls: None,
            images: None,
        }
    }

    #[test]
    fn model_supports_vision_matches_known_families() {
        assert!(model_supports_vision("gemma4:26b"));
        assert!(model_supports_vision("gemma3:12b"));
        assert!(model_supports_vision("llava:13b"));
        assert!(model_supports_vision("llama3.2-vision:11b"));
        assert!(model_supports_vision("QWEN2.5VL:7B")); // case-insensitive

        assert!(!model_supports_vision("llama3.1:8b"));
        assert!(!model_supports_vision("mistral:7b"));
        assert!(!model_supports_vision("qwen2.5:7b")); // text-only sibling
    }

    #[test]
    fn build_messages_carries_images_for_multipart() {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine;
        use rustykrab_core::types::ContentBlock;

        let png = vec![0x89u8, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
        let msg = Message {
            id: Uuid::new_v4(),
            role: Role::User,
            content: MessageContent::MultiPart(vec![
                ContentBlock::Text {
                    text: "what is this?".to_string(),
                },
                ContentBlock::Image {
                    media_type: "image/png".to_string(),
                    data: png.clone(),
                },
            ]),
            created_at: Utc::now(),
            agent_version: None,
        };

        let built = OllamaProvider::build_messages(&[msg], true).expect("build_messages");
        assert_eq!(built.len(), 1);
        assert_eq!(built[0].content.as_deref(), Some("what is this?"));
        let images = built[0].images.as_ref().expect("images present");
        assert_eq!(images.len(), 1);
        assert_eq!(images[0], STANDARD.encode(&png));
        // Images must serialize as a bare base64 string with no data-URI prefix.
        assert!(!images[0].starts_with("data:"));
    }

    #[test]
    fn build_messages_omits_images_when_none() {
        let msg = Message {
            id: Uuid::new_v4(),
            role: Role::User,
            content: MessageContent::Text("hello".to_string()),
            created_at: Utc::now(),
            agent_version: None,
        };
        let built = OllamaProvider::build_messages(&[msg], false).expect("build_messages");
        assert!(built[0].images.is_none());
        // `images` must be skipped entirely in the wire payload when absent.
        let json = serde_json::to_value(&built[0]).unwrap();
        assert!(json.get("images").is_none());
    }

    fn tool_result_with_image(png: &[u8]) -> Message {
        use rustykrab_core::types::{ContentBlock, ToolResult};
        Message {
            id: Uuid::new_v4(),
            role: Role::Tool,
            content: MessageContent::ToolResult(ToolResult {
                call_id: "call-1".to_string(),
                output: serde_json::json!({ "ok": true }),
                is_error: false,
                images: vec![ContentBlock::Image {
                    media_type: "image/png".to_string(),
                    data: png.to_vec(),
                }],
            }),
            created_at: Utc::now(),
            agent_version: None,
        }
    }

    #[test]
    fn tool_result_images_become_followup_user_message_for_vision_models() {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine;

        let png = vec![0x89u8, 0x50, 0x4e, 0x47];
        let built =
            OllamaProvider::build_messages(&[tool_result_with_image(&png)], true).expect("build");
        // One `tool` message plus an injected `user` message carrying the image.
        assert_eq!(built.len(), 2);
        assert_eq!(built[0].role, "tool");
        assert!(built[0].images.is_none());
        assert_eq!(built[1].role, "user");
        let imgs = built[1].images.as_ref().expect("follow-up images");
        assert_eq!(imgs, &vec![STANDARD.encode(&png)]);
    }

    #[test]
    fn tool_result_images_dropped_for_text_only_models() {
        let png = vec![0x89u8, 0x50, 0x4e, 0x47];
        let built =
            OllamaProvider::build_messages(&[tool_result_with_image(&png)], false).expect("build");
        // No follow-up user message when the model can't see images.
        assert_eq!(built.len(), 1);
        assert_eq!(built[0].role, "tool");
    }

    #[test]
    fn parses_context_length_from_architecture_keyed_field() {
        let raw = serde_json::json!({
            "model_info": {
                "general.architecture": "llama",
                "llama.context_length": 32768u64,
            }
        });
        assert_eq!(parse_context_length_from_show(&raw), Some(32768));
    }

    #[test]
    fn parses_context_length_from_unknown_architecture_via_suffix_match() {
        let raw = serde_json::json!({
            "model_info": {
                "general.architecture": "novel-arch",
                "novel-arch.context_length": 16384u64,
            }
        });
        assert_eq!(parse_context_length_from_show(&raw), Some(16384));
    }

    #[test]
    fn parses_kv_geometry_from_explicit_key_value_lengths() {
        let raw = serde_json::json!({
            "model_info": {
                "general.architecture": "gemma4",
                "gemma4.block_count": 62u64,
                "gemma4.attention.head_count_kv": 8u64,
                "gemma4.attention.key_length": 256u64,
                "gemma4.attention.value_length": 256u64,
            }
        });
        assert_eq!(
            parse_kv_geometry_from_show(&raw),
            Some(KvGeometry {
                layers: 62,
                kv_heads: 8,
                key_length: 256,
                value_length: 256,
            })
        );
    }

    #[test]
    fn kv_geometry_falls_back_to_embedding_over_head_count() {
        // key_length/value_length are optional in GGUF; the head dimension
        // is then embedding_length / head_count.
        let raw = serde_json::json!({
            "model_info": {
                "general.architecture": "llama",
                "llama.block_count": 32u64,
                "llama.attention.head_count_kv": 8u64,
                "llama.attention.head_count": 32u64,
                "llama.embedding_length": 4096u64,
            }
        });
        let geo = parse_kv_geometry_from_show(&raw).expect("geometry");
        assert_eq!(geo.key_length, 128);
        assert_eq!(geo.value_length, 128);
    }

    #[test]
    fn kv_geometry_takes_the_max_of_per_layer_arrays() {
        // Some GGUFs store head_count_kv per layer. Taking the max keeps the
        // estimate an upper bound rather than an optimistic one.
        let raw = serde_json::json!({
            "model_info": {
                "general.architecture": "novel",
                "novel.block_count": 4u64,
                "novel.attention.head_count_kv": [2u64, 8u64, 2u64, 4u64],
                "novel.attention.key_length": 64u64,
                "novel.attention.value_length": 64u64,
            }
        });
        assert_eq!(parse_kv_geometry_from_show(&raw).unwrap().kv_heads, 8);
    }

    #[test]
    fn kv_geometry_is_none_without_enough_metadata() {
        let raw = serde_json::json!({
            "model_info": { "general.architecture": "llama", "llama.block_count": 32u64 }
        });
        assert_eq!(parse_kv_geometry_from_show(&raw), None);
    }

    #[test]
    fn kv_cache_bytes_scale_linearly_with_window_and_element_size() {
        let geo = KvGeometry {
            layers: 62,
            kv_heads: 8,
            key_length: 256,
            value_length: 256,
        };
        // 62 layers * 8 heads * 512 dims * 2 bytes = 507,904 bytes per token.
        assert_eq!(geo.cache_bytes(1, 2), 507_904);
        assert_eq!(geo.cache_bytes(1024, 2), 507_904 * 1024);
        // q8_0 halves it.
        assert_eq!(geo.cache_bytes(1024, 1), 507_904 * 512);
    }

    #[test]
    fn returns_none_when_no_context_length_present() {
        let raw = serde_json::json!({
            "model_info": {
                "general.architecture": "llama",
            }
        });
        assert_eq!(parse_context_length_from_show(&raw), None);
    }

    #[test]
    fn returns_none_when_model_info_missing() {
        let raw = serde_json::json!({});
        assert_eq!(parse_context_length_from_show(&raw), None);
    }

    #[test]
    fn trim_returns_unchanged_when_under_budget() {
        let msgs = vec![system_msg("sys"), user_msg("hi")];
        let original_len = msgs.len();
        let trimmed = OllamaProvider::trim_to_budget(msgs, Some(8192), 1024, TEST_TOOL_TOKENS);
        assert_eq!(trimmed.len(), original_len);
    }

    #[test]
    fn trim_is_noop_when_num_ctx_is_none() {
        // No budget means defer entirely to the server; the client leaves
        // the history untouched.
        let big = "x".repeat(40_000);
        let msgs = vec![system_msg("sys"), user_msg(&big), user_msg("latest")];
        let original_len = msgs.len();
        let trimmed = OllamaProvider::trim_to_budget(msgs, None, 256, TEST_TOOL_TOKENS);
        assert_eq!(trimmed.len(), original_len);
    }

    #[test]
    fn trim_drops_oldest_messages_first_and_preserves_system() {
        // Build a conversation where each user message is ~1000 chars (~250 tokens).
        let big = "x".repeat(4000); // ~1000 tokens
        let msgs = vec![
            system_msg("you are an agent"),
            user_msg(&big),
            user_msg(&big),
            user_msg(&big),
            user_msg(&big),
            user_msg("latest"),
        ];
        // budget = 4096 - 256 - SAFETY_OVERHEAD_TOKENS(2048) = 1792 tokens
        let trimmed = OllamaProvider::trim_to_budget(msgs, Some(4096), 256, TEST_TOOL_TOKENS);
        // System message must survive.
        assert_eq!(trimmed[0].role, "system");
        // Latest message must survive.
        assert_eq!(trimmed.last().unwrap().content.as_deref(), Some("latest"));
        // Some big messages were dropped.
        assert!(trimmed.len() < 6);
    }

    #[test]
    fn trim_cuts_below_budget_so_the_next_turn_does_not_retrim() {
        // Hysteresis check: trimming to exactly-fits would make the very next
        // turn overflow again, and each trim rewrites the prompt right after
        // the system block — invalidating the whole cached prefix. One deep
        // cut must leave room for several turns of growth.
        let big = "x".repeat(4000); // ~1000 tokens each
        let mut msgs = vec![system_msg("sys")];
        for _ in 0..10 {
            msgs.push(user_msg(&big));
        }
        msgs.push(user_msg("latest"));

        let trimmed = OllamaProvider::trim_to_budget(msgs, Some(8192), 1024, TEST_TOOL_TOKENS);

        // budget = 8192 - 1024 - 2048 = 5120; target = 75% = 3840.
        let after: u32 = trimmed.iter().map(estimate_message_tokens).sum();
        assert!(
            after <= 3840,
            "expected trim to reach the 75% target, got {after} tokens"
        );

        // Re-trimming the same history must now be a no-op — that is the
        // property that keeps the prefix stable across subsequent turns.
        let len_before = trimmed.len();
        let again = OllamaProvider::trim_to_budget(trimmed, Some(8192), 1024, TEST_TOOL_TOKENS);
        assert_eq!(again.len(), len_before);
    }

    #[test]
    fn trim_never_drops_the_live_request() {
        // A single message larger than the whole target must still be sent:
        // dropping it would leave the model nothing to answer.
        let huge = "z".repeat(200_000); // ~50k tokens
        let msgs = vec![system_msg("sys"), user_msg(&huge)];
        let trimmed = OllamaProvider::trim_to_budget(msgs, Some(8192), 1024, TEST_TOOL_TOKENS);
        assert_eq!(trimmed.len(), 2);
        assert_eq!(trimmed[0].role, "system");
        assert_eq!(trimmed[1].role, "user");
    }

    #[test]
    fn trim_drops_orphan_tool_results_after_truncation() {
        let big = "y".repeat(20_000); // ~5000 tokens
        let msgs = vec![
            system_msg("sys"),
            user_msg(&big),
            tool_msg("orphaned tool result"),
            user_msg("latest"),
        ];
        let trimmed = OllamaProvider::trim_to_budget(msgs, Some(4096), 256, TEST_TOOL_TOKENS);
        // Orphan tool result must not become the first non-system message.
        assert_ne!(trimmed.get(1).map(|m| m.role.as_str()), Some("tool"));
        // System and latest user message survive.
        assert_eq!(trimmed[0].role, "system");
        assert_eq!(trimmed.last().unwrap().content.as_deref(), Some("latest"));
    }

    fn assistant_msg(content: &str) -> OllamaMessage {
        OllamaMessage {
            role: "assistant".to_string(),
            content: Some(content.to_string()),
            tool_calls: None,
            images: None,
        }
    }

    #[test]
    fn trim_always_leaves_a_user_turn() {
        // Some model chat templates reject an /api/chat request whose
        // message array carries no `user` role, so an over-long conversation
        // must degrade rather than become a hard failure. Verified against
        // Ollama 0.32.14: qwen3.8:27b returns 500 `no user query found in
        // messages`; gemma4:26b and qwen3:32b accept the same array. It is
        // the template, not the Ollama version.
        let big = "x".repeat(40_000); // ~10k tokens each
        let msgs = vec![
            system_msg("sys"),
            user_msg(&big),
            assistant_msg(&big),
            tool_msg(&big),
            assistant_msg(&big),
        ];

        let trimmed = OllamaProvider::trim_to_budget(msgs, Some(4096), 256, TEST_TOOL_TOKENS);

        assert!(
            trimmed.iter().any(|m| m.role == "user"),
            "a user turn must survive trimming, got roles {:?}",
            trimmed.iter().map(|m| &m.role).collect::<Vec<_>>()
        );
    }

    #[test]
    fn trim_leaves_no_orphan_tool_result_when_clamped_to_the_user_turn() {
        // Clamping to the last user turn must not reintroduce the orphan the
        // skip loop exists to prevent: the first surviving non-system message
        // is the user turn itself, never a dangling tool result.
        let big = "x".repeat(40_000);
        let msgs = vec![
            system_msg("sys"),
            user_msg(&big),
            assistant_msg(&big),
            tool_msg(&big),
            assistant_msg(&big),
        ];

        let trimmed = OllamaProvider::trim_to_budget(msgs, Some(4096), 256, TEST_TOOL_TOKENS);

        assert_ne!(
            trimmed.get(1).map(|m| m.role.as_str()),
            Some("tool"),
            "trimming must not leave a leading orphan tool result"
        );
    }

    #[test]
    fn trim_retains_the_oldest_user_turn_when_it_is_the_only_one() {
        // Maximal trimming pressure with the sole user turn at the front: it
        // must survive even though it is the oldest droppable message.
        let big = "x".repeat(80_000); // ~20k tokens each
        let msgs = vec![
            system_msg("sys"),
            user_msg("the only user turn"),
            assistant_msg(&big),
            assistant_msg(&big),
            assistant_msg(&big),
        ];

        let trimmed = OllamaProvider::trim_to_budget(msgs, Some(4096), 256, TEST_TOOL_TOKENS);

        assert!(
            trimmed
                .iter()
                .any(|m| m.content.as_deref() == Some("the only user turn")),
            "the sole user turn must be retained under maximal trimming"
        );
    }

    #[test]
    fn trim_does_not_strand_a_conversation_ending_in_a_tool_result() {
        // The orphan-tool skip loop is bounded by `trimmed.len()`, not by the
        // `drop_end < last` guard, so a history ending in a tool result could
        // walk past the end and drain everything but the system prompt —
        // exactly the system-only array the provider rejects.
        let big = "x".repeat(40_000);
        let msgs = vec![
            system_msg("sys"),
            user_msg(&big),
            assistant_msg(&big),
            tool_msg(&big),
        ];

        let trimmed = OllamaProvider::trim_to_budget(msgs, Some(4096), 256, TEST_TOOL_TOKENS);

        assert!(
            trimmed.len() > 1,
            "trimming must not reduce the request to the system prompt alone"
        );
        assert!(
            trimmed.iter().any(|m| m.role == "user"),
            "a user turn must survive even when the history ends in a tool result"
        );
    }

    // ---- Live tests against a real Ollama daemon -------------------------
    //
    // Ignored by default: they need a running Ollama and the model named by
    // RK_LIVE_MODEL (default qwen3.8:27b). Run with:
    //
    //   cargo test -p rustykrab-providers --  --ignored --test-threads=1
    //
    // These exist because the failure they cover is not reproducible in a
    // unit test: it lives in the model's chat template, server-side. Note
    // that the rejection is template-specific, NOT a blanket Ollama
    // behaviour — qwen3.8:27b rejects a user-less message array while
    // gemma4:26b, qwen3:32b and qwen3:30b-a3b all accept one.

    fn live_model() -> String {
        std::env::var("RK_LIVE_MODEL").unwrap_or_else(|_| "qwen3.8:27b".to_string())
    }

    fn live_provider() -> OllamaProvider {
        let mut cfg = OllamaConfig {
            num_predict: 8,
            ..Default::default()
        };
        cfg.num_ctx = Some(8192);
        OllamaProvider::new(live_model())
            .with_base_url("http://127.0.0.1:11434")
            .with_config(cfg)
    }

    fn core_msg(role: Role, text: &str) -> Message {
        Message {
            id: Uuid::new_v4(),
            role,
            content: MessageContent::Text(text.to_string()),
            created_at: Utc::now(),
            agent_version: None,
        }
    }

    /// Negative control. The exact array a resumed job conversation produced
    /// before the fix — system + assistant, no user turn — must still be
    /// rejected by the live model. If this ever starts passing, the premise
    /// behind the fix has changed and the other live tests prove nothing.
    #[tokio::test]
    #[ignore = "requires a live Ollama daemon"]
    async fn live_resumed_job_shape_without_a_user_turn_is_rejected() {
        let provider = live_provider();
        let msgs = vec![
            core_msg(Role::System, "You are a helpful assistant."),
            core_msg(Role::Assistant, "result of the previous scheduled run"),
        ];

        let err = provider
            .chat(&msgs, &[])
            .await
            .expect_err("a user-less array must be rejected by the live model");

        let text = err.to_string();
        assert!(
            text.contains("no user query found in messages"),
            "expected the documented provider rejection, got: {text}"
        );
    }

    /// Fix 1, end to end: the same conversation with the scheduled prompt
    /// appended as a real user turn goes through the live model.
    #[tokio::test]
    #[ignore = "requires a live Ollama daemon"]
    async fn live_resumed_job_shape_with_a_user_turn_succeeds() {
        let provider = live_provider();
        let msgs = vec![
            core_msg(Role::System, "You are a helpful assistant."),
            core_msg(Role::Assistant, "result of the previous scheduled run"),
            core_msg(
                Role::User,
                "[Scheduled task] Execute it and reply concisely.",
            ),
        ];

        provider
            .chat(&msgs, &[])
            .await
            .expect("appending the scheduled user turn must make the request valid");
    }

    /// Fix 2 on the shape production actually produces once Fix 1 is in
    /// place: a long history whose newest message is the scheduled user turn.
    /// Trimming must drop the old bulk and keep the request valid.
    #[tokio::test]
    #[ignore = "requires a live Ollama daemon"]
    async fn live_overlong_history_ending_in_a_user_turn_succeeds() {
        let provider = live_provider();
        let big = "x".repeat(40_000); // ~10k tokens each

        let mut msgs = vec![core_msg(Role::System, "You are a helpful assistant.")];
        msgs.push(core_msg(Role::User, "an old user turn"));
        for _ in 0..3 {
            msgs.push(core_msg(Role::Assistant, &big));
        }
        // Fix 1 puts the scheduled prompt last; this is the live shape.
        msgs.push(core_msg(
            Role::User,
            "[Scheduled task] Execute it and reply concisely.",
        ));

        provider
            .chat(&msgs, &[])
            .await
            .expect("a history ending in a user turn must survive trimming");
    }

    /// KNOWN LIMITATION, asserted so it cannot regress silently.
    ///
    /// When the last user turn is old and large content follows it, the clamp
    /// pins `drop_end` to that turn and the trimmer — which only drops from
    /// the front — can free nothing. The over-budget request is then truncated
    /// server-side, oldest-first, which discards the user turn and reproduces
    /// the very rejection the clamp exists to prevent. `trim_to_budget` logs
    /// this via the `error!` branch but cannot currently fix it: escaping it
    /// requires dropping from the middle, which trades away the KV-cache
    /// prefix stability #488 was built for.
    ///
    /// Flip this assertion to `expect(...)` when that is addressed.
    #[tokio::test]
    #[ignore = "requires a live Ollama daemon"]
    async fn live_old_user_turn_followed_by_bulk_is_a_known_failure() {
        let provider = live_provider();
        let big = "x".repeat(40_000);

        let mut msgs = vec![
            core_msg(Role::System, "You are a helpful assistant."),
            core_msg(Role::User, "the only user turn"),
        ];
        for _ in 0..3 {
            msgs.push(core_msg(Role::Assistant, &big));
        }

        let err = provider
            .chat(&msgs, &[])
            .await
            .expect_err("documented limitation: an old user turn buried under bulk still fails");
        assert!(
            err.to_string().contains("no user query found in messages"),
            "expected the documented rejection, got: {err}"
        );
    }

    /// Run `f` with the two num_ctx env vars set to `rk` / `ollama`.
    ///
    /// `std::env` is process-global and `cargo test` is multi-threaded, so
    /// every test that touches these vars serialises on one mutex and
    /// restores the prior values on the way out.
    fn with_num_ctx_env<T>(rk: Option<&str>, ollama: Option<&str>, f: impl FnOnce() -> T) -> T {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let saved_rk = std::env::var("RUSTYKRAB_NUM_CTX").ok();
        let saved_ollama = std::env::var("OLLAMA_NUM_CTX").ok();
        // SAFETY: all writers of these vars hold ENV_LOCK for the duration.
        unsafe {
            match rk {
                Some(v) => std::env::set_var("RUSTYKRAB_NUM_CTX", v),
                None => std::env::remove_var("RUSTYKRAB_NUM_CTX"),
            }
            match ollama {
                Some(v) => std::env::set_var("OLLAMA_NUM_CTX", v),
                None => std::env::remove_var("OLLAMA_NUM_CTX"),
            }
        }
        let out = f();
        unsafe {
            match saved_rk {
                Some(v) => std::env::set_var("RUSTYKRAB_NUM_CTX", v),
                None => std::env::remove_var("RUSTYKRAB_NUM_CTX"),
            }
            match saved_ollama {
                Some(v) => std::env::set_var("OLLAMA_NUM_CTX", v),
                None => std::env::remove_var("OLLAMA_NUM_CTX"),
            }
        }
        out
    }

    #[test]
    fn default_config_pins_num_ctx_when_env_unset() {
        // Pinning one stable window is what lets Ollama keep a warm runner
        // across requests, so every preset must land on the same value.
        with_num_ctx_env(None, None, || {
            assert_eq!(OllamaConfig::default().num_ctx, Some(DEFAULT_NUM_CTX));
            assert_eq!(OllamaConfig::tool_calling().num_ctx, Some(DEFAULT_NUM_CTX));
            assert_eq!(OllamaConfig::creative().num_ctx, Some(DEFAULT_NUM_CTX));
        });
    }

    #[test]
    fn num_ctx_env_override_is_honoured_with_rustykrab_taking_precedence() {
        with_num_ctx_env(Some("16384"), None, || {
            assert_eq!(num_ctx_from_env(), Some(16384));
        });
        with_num_ctx_env(None, Some("8192"), || {
            assert_eq!(num_ctx_from_env(), Some(8192));
        });
        with_num_ctx_env(Some("16384"), Some("8192"), || {
            assert_eq!(num_ctx_from_env(), Some(16384));
        });
    }

    #[test]
    fn num_ctx_server_sentinel_defers_to_ollama() {
        for sentinel in ["server", "SERVER", "default", "0", " "] {
            with_num_ctx_env(Some(sentinel), None, || {
                assert_eq!(num_ctx_from_env(), None, "sentinel {sentinel:?}");
            });
        }
    }

    #[test]
    fn unparseable_num_ctx_falls_back_to_default_rather_than_deferring() {
        with_num_ctx_env(Some("thirty-two thousand"), None, || {
            assert_eq!(num_ctx_from_env(), Some(DEFAULT_NUM_CTX));
        });
    }

    #[test]
    fn model_supports_thinking_matches_known_families() {
        assert!(model_supports_thinking("gemma4:26b"));
        assert!(model_supports_thinking("qwen3:32b"));
        assert!(model_supports_thinking("DeepSeek-R1:14b")); // case-insensitive

        // Sending `think` to these would be a 400 from Ollama.
        assert!(!model_supports_thinking("llama3.1:8b"));
        assert!(!model_supports_thinking("mistral:7b"));
        assert!(!model_supports_thinking("qwen2.5:7b"));
    }

    #[test]
    fn compaction_threshold_sits_below_the_trimming_budget() {
        // The regression this guards: compaction (summarize + archive) must
        // fire before trimming (drop the oldest turns outright). The runner
        // compacts at 85% of what `context_limit()` reports, so that figure
        // has to be the *usable* budget, not the raw window — otherwise the
        // threshold lands above the trim budget and trimming always wins.
        for window in [8_192u32, 16_384, 32_768, 65_536, 131_072] {
            let provider = OllamaProvider::new("probe").with_config(OllamaConfig {
                num_ctx: Some(window),
                num_predict: 4096,
                ..OllamaConfig::default()
            });

            let reported = provider.context_limit().expect("limit") as u32;
            let compaction_threshold = (reported as f64 * 0.85) as u32;
            let trim_budget = input_budget(window, 4096, ASSUMED_TOOL_TOKENS);

            assert!(
                compaction_threshold < trim_budget,
                "window {window}: compaction at {compaction_threshold} must precede \
                 trimming at {trim_budget}"
            );
        }
    }

    #[test]
    fn context_limit_excludes_output_and_overhead_reservations() {
        let provider = OllamaProvider::new("probe").with_config(OllamaConfig {
            num_ctx: Some(32_768),
            num_predict: 4096,
            ..OllamaConfig::default()
        });
        let expected = 32_768 - 4096 - ASSUMED_TOOL_TOKENS - FRAMING_OVERHEAD_TOKENS;
        assert_eq!(provider.context_limit(), Some(expected as usize));
    }

    #[test]
    fn context_limit_floors_when_reservations_exceed_the_window() {
        // A window smaller than the reservations must not report 0, which
        // would collapse every downstream budget to nothing. It must not
        // report `None` either: the caller reads that as "unknown" and
        // falls back to the profile's max_context_tokens — a number this
        // provider will never honour, because the per-request path still
        // trims against the real budget. Compaction then never fires and
        // history is silently trimmed away instead of summarised.
        let provider = OllamaProvider::new("probe").with_config(OllamaConfig {
            num_ctx: Some(1024),
            num_predict: 4096,
            ..OllamaConfig::default()
        });
        let limit = provider
            .context_limit()
            .expect("a configured window reports something");
        assert!(limit > 0, "0 would collapse every downstream budget");
        assert!(limit < 1024, "the floor stays below the window");
    }

    #[test]
    fn context_limit_floor_is_a_quarter_of_the_window() {
        let provider = OllamaProvider::new("probe").with_config(OllamaConfig {
            num_ctx: Some(6144),
            num_predict: 4096,
            ..OllamaConfig::default()
        });
        assert_eq!(provider.context_limit(), Some(1536));
    }

    #[test]
    fn trim_budget_shrinks_as_the_tool_block_grows() {
        // Tool schemas are part of the prompt, so loading more tools has to
        // leave less room for history. The old flat 2048 reservation missed
        // this entirely: a conversation with the full catalog loaded could be
        // "trimmed" and still overflow the window.
        let big = "x".repeat(4000); // ~1000 tokens each
        let build = || {
            let mut msgs = vec![system_msg("sys")];
            for _ in 0..10 {
                msgs.push(user_msg(&big));
            }
            msgs.push(user_msg("latest"));
            msgs
        };

        let few = OllamaProvider::trim_to_budget(build(), Some(16_384), 1024, 500);
        let many = OllamaProvider::trim_to_budget(build(), Some(16_384), 1024, 10_000);
        assert!(
            many.len() < few.len(),
            "a 10k-token tool block must force more history out than a 500-token one \
             (kept {} vs {})",
            many.len(),
            few.len()
        );
    }

    #[test]
    fn estimate_tool_tokens_tracks_serialized_size() {
        let tools = vec![OllamaTool {
            r#type: "function".to_string(),
            function: OllamaToolDef {
                name: "read".to_string(),
                description: "Read a file".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"],
                }),
            },
        }];
        let serialized_len = serde_json::to_string(&tools).unwrap().len();
        assert_eq!(
            estimate_tool_tokens(&tools),
            serialized_len.div_ceil(CHARS_PER_TOKEN) as u32
        );
    }

    #[test]
    fn estimate_handles_multibyte_characters() {
        // 4 multibyte chars should count as ceil(4/4) = 1 token, not 12 (their byte length).
        let tokens = estimate_text_tokens("日本語x");
        assert_eq!(tokens, 1);
    }

    #[test]
    fn estimate_json_tokens_matches_serialized_length_without_allocating() {
        let v = serde_json::json!({ "path": "/tmp/file.txt", "recursive": true });
        let serialized_len = v.to_string().len();
        assert_eq!(
            estimate_json_tokens(&v),
            serialized_len.div_ceil(CHARS_PER_TOKEN) as u32
        );
    }

    #[test]
    fn ndjson_stream_survives_chunk_split_inside_multibyte_char() {
        // Two NDJSON chunks whose content contains multi-byte characters,
        // delivered with a network-chunk boundary inside a codepoint. The
        // byte-level line buffer must reassemble it without U+FFFD.
        let payload = "{\"message\":{\"content\":\"héllo\"},\"done\":false}\n\
                       {\"message\":{\"content\":\" wörld\"},\"done\":true,\"done_reason\":\"stop\"}\n";
        let bytes = payload.as_bytes();
        // Split one byte into the "é" (0xC3 0xA9).
        let split = payload.find('é').unwrap() + 1;

        let mut buffer = LineBuffer::new();
        let mut full_text = String::new();
        let mut done_reason: Option<String> = None;
        for chunk in [&bytes[..split], &bytes[split..]] {
            buffer.push_chunk(chunk);
            while let Some(line) = buffer.next_line() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let stream_chunk: OllamaStreamChunk = serde_json::from_str(line).expect("parse");
                if let Some(content) = stream_chunk.message.content {
                    full_text.push_str(&content);
                }
                if stream_chunk.done {
                    done_reason = stream_chunk.done_reason;
                }
            }
        }
        assert_eq!(full_text, "héllo wörld");
        assert_eq!(done_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn ndjson_multiple_lines_in_one_chunk_parse_in_order() {
        let payload = "{\"message\":{\"content\":\"a\"},\"done\":false}\n\
                       {\"message\":{\"content\":\"b\"},\"done\":false}\n\
                       {\"message\":{\"content\":\"c\"},\"done\":true}\n";
        let mut buffer = LineBuffer::new();
        buffer.push_chunk(payload.as_bytes());

        let mut full_text = String::new();
        let mut saw_done = false;
        while let Some(line) = buffer.next_line() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let stream_chunk: OllamaStreamChunk = serde_json::from_str(line).expect("parse");
            if let Some(content) = stream_chunk.message.content {
                full_text.push_str(&content);
            }
            saw_done |= stream_chunk.done;
        }
        assert_eq!(full_text, "abc");
        assert!(saw_done);
        assert_eq!(buffer.len(), 0);
    }
}
