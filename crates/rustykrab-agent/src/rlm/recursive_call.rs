//! Recursive call tree execution.
//!
//! The model can request sub-LLM calls by emitting a special format
//! in its response. Each sub-call gets its own focused context window.
//! Results are summarized before injection back into the parent context.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use rustykrab_core::model::ModelProvider;
use rustykrab_core::orchestration::{OrchestrationConfig, RecursiveCall};
use rustykrab_core::types::{Message, MessageContent, Role};
use rustykrab_core::{Error, Result};
use uuid::Uuid;

use super::context_manager::ContextManager;

/// Executes recursive call trees where the model can delegate sub-queries.
pub struct RecursiveExecutor {
    provider: Arc<dyn ModelProvider>,
    config: OrchestrationConfig,
}

/// Marker format the model uses to request a sub-call.
/// The model emits: [SUB_CALL: <question>]
const SUB_CALL_PREFIX: &str = "[SUB_CALL:";
const SUB_CALL_SUFFIX: &str = "]";

impl RecursiveExecutor {
    pub fn new(provider: Arc<dyn ModelProvider>, config: OrchestrationConfig) -> Self {
        Self { provider, config }
    }

    /// Execute a recursive query, allowing the model to delegate sub-queries.
    pub async fn execute(&self, prompt: &str, context: Option<&str>) -> Result<String> {
        tracing::info!(
            budget = self.config.sub_task_context_budget,
            max_depth = self.config.max_recursion_depth,
            prompt_len = prompt.len(),
            "RLM: starting recursive execution"
        );
        let start = std::time::Instant::now();
        let root = RecursiveCall::root(prompt, self.config.sub_task_context_budget);
        let result = execute_call(
            self.provider.clone(),
            self.config.clone(),
            root,
            context.map(|s| s.to_string()),
        )
        .await;
        let elapsed = start.elapsed();
        match &result {
            Ok(text) => tracing::info!(
                duration_ms = elapsed.as_millis() as u64,
                response_len = text.len(),
                "RLM: recursive execution completed"
            ),
            Err(e) => tracing::error!(
                duration_ms = elapsed.as_millis() as u64,
                error = %e,
                "RLM: recursive execution failed"
            ),
        }
        result
    }
}

/// Execute a single call in the recursive tree.
///
/// This is a free function (not a method) so it can be spawned into
/// tokio tasks without lifetime issues. Returns a boxed future to
/// enable recursive async calls with `Send` bound.
fn execute_call(
    provider: Arc<dyn ModelProvider>,
    config: OrchestrationConfig,
    call: RecursiveCall,
    context: Option<String>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send>> {
    Box::pin(execute_call_impl(provider, config, call, context))
}

async fn execute_call_impl(
    provider: Arc<dyn ModelProvider>,
    config: OrchestrationConfig,
    call: RecursiveCall,
    context: Option<String>,
) -> Result<String> {
    let context_mgr = ContextManager::new(config.clone());

    tracing::info!(
        depth = call.depth,
        budget = call.context_budget,
        prompt_preview = &call.prompt[..call.prompt.len().min(100)],
        "RLM: executing call"
    );

    if call.depth >= context_mgr.max_depth() {
        tracing::warn!(
            depth = call.depth,
            "RLM: max recursion depth reached, answering directly"
        );
        return direct_call(&provider, &call.prompt, context.as_deref()).await;
    }

    let budget = context_mgr.child_budget(call.context_budget, call.depth);
    if budget == 0 {
        tracing::warn!(depth = call.depth, "RLM: budget exhausted, answering directly");
        return direct_call(&provider, &call.prompt, context.as_deref()).await;
    }

    // Build the prompt instructing the model it can delegate.
    let system = format!(
        "You are a reasoning engine. Answer the question below.\n\
         If you need to break a sub-question out for separate analysis, \
         wrap it in [SUB_CALL: your sub-question here].\n\
         Sub-calls will be resolved and their results substituted back.\n\
         You can use up to 3 sub-calls per response.\n\
         Current depth: {}/{}\n\
         Context budget: {} tokens",
        call.depth,
        context_mgr.max_depth(),
        budget,
    );

    let mut messages = vec![Message {
        id: Uuid::new_v4(),
        role: Role::System,
        content: MessageContent::Text(system),
        created_at: Utc::now(),
    }];

    if let Some(ref ctx) = context {
        let ctx = ContextManager::truncate_to_budget(ctx, budget / 3);
        messages.push(Message {
            id: Uuid::new_v4(),
            role: Role::System,
            content: MessageContent::Text(format!("Context:\n{ctx}")),
            created_at: Utc::now(),
        });
    }

    messages.push(Message {
        id: Uuid::new_v4(),
        role: Role::User,
        content: MessageContent::Text(call.prompt.clone()),
        created_at: Utc::now(),
    });

    let response = provider.chat(&messages, &[]).await?;
    let text = response
        .message
        .content
        .as_text()
        .ok_or_else(|| Error::Internal("recursive call returned non-text".into()))?;

    // Check for sub-call requests.
    let sub_calls = extract_sub_calls(text);
    if sub_calls.is_empty() {
        tracing::debug!(depth = call.depth, "RLM: no sub-calls requested");
        return Ok(text.to_string());
    }

    let sub_call_previews: Vec<&str> = sub_calls
        .iter()
        .map(|s| &s[..s.len().min(80)])
        .collect();
    tracing::info!(
        depth = call.depth,
        sub_calls = sub_calls.len(),
        prompts = ?sub_call_previews,
        "RLM: model requested sub-calls"
    );

    // Execute sub-calls concurrently.
    let mut handles = Vec::new();
    for sub_prompt in &sub_calls {
        let child = RecursiveCall::child(
            call.id,
            sub_prompt.clone(),
            context_mgr.child_budget(budget, call.depth + 1),
            call.depth + 1,
        );
        let provider = provider.clone();
        let config = config.clone();

        handles.push(tokio::spawn(
            execute_call(provider, config, child, None),
        ));
    }

    // Collect results.
    let mut sub_results: HashMap<String, String> = HashMap::new();
    for (i, handle) in handles.into_iter().enumerate() {
        let result = match handle.await {
            Ok(Ok(text)) => {
                tracing::info!(
                    depth = call.depth,
                    sub_call_index = i,
                    result_len = text.len(),
                    "RLM: sub-call completed"
                );
                text
            }
            Ok(Err(e)) => {
                tracing::error!(
                    depth = call.depth,
                    sub_call_index = i,
                    error = %e,
                    "RLM: sub-call failed"
                );
                format!("[Error resolving sub-call: {e}]")
            }
            Err(e) => {
                tracing::error!(
                    depth = call.depth,
                    sub_call_index = i,
                    error = %e,
                    "RLM: sub-call panicked"
                );
                format!("[Sub-call panicked: {e}]")
            }
        };
        sub_results.insert(sub_calls[i].clone(), result);
    }

    tracing::info!(
        depth = call.depth,
        resolved_count = sub_results.len(),
        "RLM: all sub-calls resolved, substituting results"
    );

    // Substitute results back into the original response.
    let mut resolved = text.to_string();
    for (prompt, result) in &sub_results {
        let marker = format!("{SUB_CALL_PREFIX} {prompt}{SUB_CALL_SUFFIX}");
        let summary = ContextManager::truncate_to_budget(result, budget / 4);
        resolved = resolved.replace(&marker, &summary);
    }

    // Clean up any remaining unresolved markers.
    if resolved.contains(SUB_CALL_PREFIX) {
        resolved = remove_unresolved_markers(&resolved);
    }

    Ok(resolved)
}

/// Direct model call without sub-call support.
async fn direct_call(
    provider: &Arc<dyn ModelProvider>,
    prompt: &str,
    context: Option<&str>,
) -> Result<String> {
    let mut messages = Vec::new();
    if let Some(ctx) = context {
        messages.push(Message {
            id: Uuid::new_v4(),
            role: Role::System,
            content: MessageContent::Text(ctx.to_string()),
            created_at: Utc::now(),
        });
    }
    messages.push(Message {
        id: Uuid::new_v4(),
        role: Role::User,
        content: MessageContent::Text(prompt.to_string()),
        created_at: Utc::now(),
    });

    let response = provider.chat(&messages, &[]).await?;
    Ok(response
        .message
        .content
        .as_text()
        .unwrap_or("")
        .to_string())
}

/// Extract sub-call prompts from model output.
fn extract_sub_calls(text: &str) -> Vec<String> {
    let mut calls = Vec::new();
    let mut remaining = text;

    while let Some(start) = remaining.find(SUB_CALL_PREFIX) {
        let after_prefix = &remaining[start + SUB_CALL_PREFIX.len()..];
        if let Some(end) = after_prefix.find(SUB_CALL_SUFFIX) {
            let prompt = after_prefix[..end].trim().to_string();
            if !prompt.is_empty() {
                calls.push(prompt);
            }
            remaining = &after_prefix[end + SUB_CALL_SUFFIX.len()..];
        } else {
            break;
        }
    }

    // Limit to 3 sub-calls per response.
    calls.truncate(3);
    calls
}

/// Remove unresolved [SUB_CALL: ...] markers from text.
fn remove_unresolved_markers(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut remaining = text;

    while let Some(start) = remaining.find(SUB_CALL_PREFIX) {
        result.push_str(&remaining[..start]);
        let after_prefix = &remaining[start + SUB_CALL_PREFIX.len()..];
        if let Some(end) = after_prefix.find(SUB_CALL_SUFFIX) {
            remaining = &after_prefix[end + SUB_CALL_SUFFIX.len()..];
        } else {
            remaining = after_prefix;
        }
    }
    result.push_str(remaining);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_sub_calls() {
        let text = "I need to check two things: \
                    [SUB_CALL: What is the weather in Tokyo?] \
                    and also [SUB_CALL: What time is it in London?]";
        let calls = extract_sub_calls(text);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], "What is the weather in Tokyo?");
        assert_eq!(calls[1], "What time is it in London?");
    }

    #[test]
    fn test_extract_sub_calls_empty() {
        let text = "No sub-calls here, just a regular response.";
        let calls = extract_sub_calls(text);
        assert!(calls.is_empty());
    }

    #[test]
    fn test_extract_sub_calls_max_three() {
        let text = "[SUB_CALL: a] [SUB_CALL: b] [SUB_CALL: c] [SUB_CALL: d]";
        let calls = extract_sub_calls(text);
        assert_eq!(calls.len(), 3);
    }

    #[test]
    fn test_remove_unresolved_markers() {
        let text = "Here [SUB_CALL: something] and there";
        let cleaned = remove_unresolved_markers(text);
        assert_eq!(cleaned, "Here  and there");
    }
}
