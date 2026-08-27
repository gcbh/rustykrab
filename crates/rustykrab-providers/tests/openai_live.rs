//! Live tests against a real OpenAI-compatible server.
//!
//! Ignored by default so CI needs no server. Run them against whatever backend
//! you are evaluating:
//!
//! ```sh
//! # Ollama's OpenAI-compatible endpoint
//! LIVE_OPENAI_BASE_URL=http://localhost:11434/v1 LIVE_OPENAI_MODEL=qwen3:30b-a3b \
//!   cargo test -p rustykrab-providers --test openai_live -- --ignored --nocapture
//!
//! # llama.cpp
//! LIVE_OPENAI_BASE_URL=http://localhost:8080 cargo test ... -- --ignored --nocapture
//! ```

use std::sync::{Arc, Mutex};

use rustykrab_core::model::{ModelProvider, StopReason, StreamEvent};
use rustykrab_core::types::{Message, MessageContent, Role, ToolResult, ToolSchema};
use rustykrab_providers::{OpenAiConfig, OpenAiProvider};
use uuid::Uuid;

fn provider() -> OpenAiProvider {
    let base_url = std::env::var("LIVE_OPENAI_BASE_URL")
        .unwrap_or_else(|_| "http://localhost:11434/v1".to_string());
    let model = std::env::var("LIVE_OPENAI_MODEL").unwrap_or_else(|_| "qwen3:30b-a3b".to_string());
    eprintln!("--> {base_url} model={model}");
    OpenAiProvider::new(model)
        .with_base_url(base_url)
        .with_config(OpenAiConfig::tool_calling())
}

fn msg(role: Role, content: MessageContent) -> Message {
    Message {
        id: Uuid::new_v4(),
        role,
        content,
        created_at: chrono::Utc::now(),
        agent_version: None,
    }
}

fn user(text: &str) -> Message {
    msg(Role::User, MessageContent::Text(text.into()))
}

fn weather_tool() -> ToolSchema {
    ToolSchema {
        name: "get_weather".into(),
        description: "Get the current weather for a city.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "city": {"type": "string", "description": "City name"}
            },
            "required": ["city"]
        }),
    }
}

#[tokio::test]
#[ignore = "requires a running OpenAI-compatible server"]
async fn live_text_generation() {
    let resp = provider()
        .chat(
            &[user("Reply with exactly the word: pong. Nothing else.")],
            &[],
        )
        .await
        .expect("chat failed");

    let text = resp.message.content.as_text().unwrap_or_default();
    eprintln!("text: {text:?}");
    eprintln!("usage: {:?}", resp.usage);

    assert!(!text.is_empty(), "model returned no text");
    assert_eq!(resp.stop_reason, StopReason::EndTurn);
    assert!(
        resp.usage.prompt_tokens > 0,
        "no prompt token usage reported"
    );
    assert!(
        resp.usage.completion_tokens > 0,
        "no completion token usage reported"
    );
}

#[tokio::test]
#[ignore = "requires a running OpenAI-compatible server"]
async fn live_streaming_emits_incremental_deltas() {
    let deltas = Arc::new(Mutex::new(Vec::<String>::new()));
    let done = Arc::new(Mutex::new(0usize));
    let (d, dn) = (deltas.clone(), done.clone());

    let resp = provider()
        .chat_stream(
            &[user(
                "Count from 1 to 10, separated by spaces. No other text.",
            )],
            &[],
            &move |ev| match ev {
                StreamEvent::TextDelta(t) => d.lock().unwrap().push(t),
                StreamEvent::Done(_) => *dn.lock().unwrap() += 1,
            },
        )
        .await
        .expect("chat_stream failed");

    let deltas = deltas.lock().unwrap().clone();
    eprintln!("delta count: {}", deltas.len());
    eprintln!("assembled: {:?}", resp.message.content.as_text());
    eprintln!("usage: {:?}", resp.usage);

    assert!(
        deltas.len() > 1,
        "expected incremental deltas, got {}",
        deltas.len()
    );
    assert_eq!(*done.lock().unwrap(), 1, "expected exactly one Done event");
    // The assembled text must equal the concatenated deltas.
    assert_eq!(
        resp.message.content.as_text().unwrap_or_default(),
        deltas.concat()
    );
    assert!(
        resp.usage.completion_tokens > 0,
        "no usage in stream trailer"
    );
}

#[tokio::test]
#[ignore = "requires a running OpenAI-compatible server"]
async fn live_tool_call_is_parsed() {
    let resp = provider()
        .chat(
            &[user("What is the weather in Paris? Use the tool.")],
            &[weather_tool()],
        )
        .await
        .expect("chat failed");

    eprintln!("stop_reason: {:?}", resp.stop_reason);
    eprintln!("text alongside: {:?}", resp.text);

    assert_eq!(
        resp.stop_reason,
        StopReason::ToolUse,
        "model did not request a tool call"
    );
    let calls = resp.message.content.tool_calls();
    assert_eq!(calls.len(), 1);
    eprintln!("call: {} {}", calls[0].name, calls[0].arguments);

    assert_eq!(calls[0].name, "get_weather");
    assert!(!calls[0].id.is_empty(), "tool call must carry an id");
    // Arguments must have decoded from the wire string into a real object.
    assert!(
        calls[0].arguments.is_object(),
        "arguments did not decode to an object: {}",
        calls[0].arguments
    );
    let city = calls[0].arguments["city"].as_str().unwrap_or_default();
    assert!(
        city.to_lowercase().contains("paris"),
        "unexpected city argument: {city:?}"
    );
}

#[tokio::test]
#[ignore = "requires a running OpenAI-compatible server"]
async fn live_streaming_tool_call_is_reassembled() {
    let resp = provider()
        .chat_stream(
            &[user("What is the weather in Tokyo? Use the tool.")],
            &[weather_tool()],
            &|_| {},
        )
        .await
        .expect("chat_stream failed");

    assert_eq!(resp.stop_reason, StopReason::ToolUse);
    let calls = resp.message.content.tool_calls();
    assert_eq!(calls.len(), 1);
    eprintln!("streamed call: {} {}", calls[0].name, calls[0].arguments);

    assert_eq!(calls[0].name, "get_weather");
    assert!(
        calls[0].arguments.is_object(),
        "streamed arguments did not reassemble into an object: {}",
        calls[0].arguments
    );
    let city = calls[0].arguments["city"].as_str().unwrap_or_default();
    assert!(
        city.to_lowercase().contains("tokyo"),
        "unexpected city argument: {city:?}"
    );
}

/// The full agent-loop shape: model calls a tool, we feed the result back
/// referencing the call id, and the model answers using it. This is what
/// breaks if `tool_call_id` correlation is wrong.
#[tokio::test]
#[ignore = "requires a running OpenAI-compatible server"]
async fn live_tool_result_round_trip() {
    let p = provider();
    let tools = [weather_tool()];

    let mut conversation = vec![user("What is the weather in Paris? Use the tool.")];

    let first = p.chat(&conversation, &tools).await.expect("turn 1 failed");
    assert_eq!(
        first.stop_reason,
        StopReason::ToolUse,
        "no tool call issued"
    );
    let call = first.message.content.tool_calls()[0].clone();
    eprintln!("turn 1 -> {}({})", call.name, call.arguments);

    // Echo the assistant turn back, then answer the call by id.
    conversation.push(first.message.clone());
    conversation.push(msg(
        Role::Tool,
        MessageContent::ToolResult(ToolResult {
            call_id: call.id.clone(),
            output: serde_json::json!({"temp_c": 3, "conditions": "freezing fog"}),
            is_error: false,
            images: Vec::new(),
        }),
    ));

    let second = p.chat(&conversation, &tools).await.expect("turn 2 failed");
    let text = second
        .message
        .content
        .as_text()
        .or(second.text.as_deref())
        .unwrap_or_default();
    eprintln!("turn 2 -> {text:?}");

    assert!(!text.is_empty(), "model gave no final answer");
    // The model can only know these from the tool result we supplied.
    let lower = text.to_lowercase();
    assert!(
        lower.contains("fog") || lower.contains("3"),
        "final answer does not reflect the tool result: {text:?}"
    );
}
