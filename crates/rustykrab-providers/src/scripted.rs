//! Deterministic, replayable model provider for end-to-end tests.
//!
//! `ScriptedProvider` never talks to a model. It matches the latest user
//! message against a list of scenarios loaded from a JSON script and
//! replays that scenario's steps — tool calls and text turns — one step
//! per `chat()` call. Scenarios like "the agent tries to overwrite
//! `notion_api_token`" thus run deterministically, fast, and free.
//!
//! Selected at daemon startup with `RUSTYKRAB_PROVIDER=scripted` and
//! `RUSTYKRAB_SCRIPT_PATH=<file.json>`. Script format:
//!
//! ```json
//! {
//!   "defaultText": "Done.",
//!   "scenarios": [
//!     {
//!       "trigger": "e2e: create credential",
//!       "steps": [
//!         { "toolCalls": [ { "name": "credential_write",
//!                            "arguments": { "action": "set",
//!                                           "name": "e2e_token",
//!                                           "value": "s3cret" } } ] },
//!         { "toolCalls": [ { "name": "task_complete",
//!                            "arguments": { "summary": "Created e2e_token." } } ] }
//!       ]
//!     }
//!   ]
//! }
//! ```
//!
//! Note the closing `task_complete` rather than a bare text step: once a
//! run has called any tool, the agent runner treats text alone as an
//! incomplete turn and re-prompts, so a trailing text step would be
//! replaced by the reminder loop instead of becoming the final message.
//!
//! Matching is stateless: on every `chat()` call the provider finds the
//! **last user message that matches a scenario trigger** and counts the
//! assistant turns after it to know which step to emit next. Anchoring on
//! the matching message (rather than simply the last user message) is
//! deliberate — the agent runner injects its own user-role nudges
//! ("Continue.", the `task_complete` reminder) mid-run, and those must not
//! reset the replay. Once a scenario's steps are exhausted — or when no
//! scenario matches (e.g. internal classifier or memory-extraction calls
//! sharing the provider) — it answers with `defaultText` and ends the turn.
//!
//! A scenario whose work is done should finish with a `task_complete`
//! tool call: that is the runner's explicit completion signal, and its
//! `summary` becomes the final assistant message.
//!
//! **Known limitation:** because the step index is derived from the
//! transcript rather than held as state, anything that *rewrites* history
//! desynchronises the replay — notably compaction, which collapses the
//! conversation into a summary and would restart the scenario mid-run.
//! Scenarios are therefore meant to be short (a handful of turns, well
//! under the compaction threshold). If a scenario ever needs to be long
//! enough to compact, give the provider explicit state instead of
//! extending this counting scheme.

use async_trait::async_trait;
use chrono::Utc;
use serde::Deserialize;
use uuid::Uuid;

use rustykrab_core::model::{ModelProvider, ModelResponse, StopReason, Usage};
use rustykrab_core::types::{Message, MessageContent, Role, ToolCall, ToolSchema};
use rustykrab_core::{Error, Result};

/// One scripted assistant turn: tool calls, text, or both. When both are
/// present the text rides along like a mixed Anthropic response (text
/// preserved in `ModelResponse::text`, tool calls in the message).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScriptStep {
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ScriptToolCall>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptToolCall {
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Scenario {
    /// Substring matched against the latest user message.
    pub trigger: String,
    pub steps: Vec<ScriptStep>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Script {
    /// Reply for unmatched or exhausted turns. Ends the turn.
    #[serde(default = "default_text")]
    pub default_text: String,
    #[serde(default)]
    pub scenarios: Vec<Scenario>,
}

fn default_text() -> String {
    "Done.".to_string()
}

pub struct ScriptedProvider {
    script: Script,
}

impl ScriptedProvider {
    pub fn new(script: Script) -> Self {
        Self { script }
    }

    pub fn from_json(json: &str) -> Result<Self> {
        let script: Script = serde_json::from_str(json)
            .map_err(|e| Error::Internal(format!("invalid scripted-provider script: {e}")))?;
        // A step with neither text nor tool calls would silently replay as
        // a turn-ending default — almost always an editing mistake, and one
        // that would make a scenario fail for a confusing reason.
        for scenario in &script.scenarios {
            if scenario.steps.is_empty() {
                return Err(Error::Internal(format!(
                    "scripted scenario '{}' has no steps",
                    scenario.trigger
                )));
            }
            for (i, step) in scenario.steps.iter().enumerate() {
                if step.text.is_none() && step.tool_calls.is_empty() {
                    return Err(Error::Internal(format!(
                        "scripted scenario '{}' step {i} has neither text nor toolCalls",
                        scenario.trigger
                    )));
                }
            }
        }
        Ok(Self::new(script))
    }

    pub fn from_path(path: &std::path::Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| Error::Internal(format!("cannot read script {}: {e}", path.display())))?;
        Self::from_json(&raw)
    }

    fn text_response(&self, text: &str) -> ModelResponse {
        ModelResponse {
            message: assistant_message(MessageContent::Text(text.to_string())),
            usage: Usage::default(),
            stop_reason: StopReason::EndTurn,
            text: None,
        }
    }

    fn step_response(&self, step: &ScriptStep) -> ModelResponse {
        if step.tool_calls.is_empty() {
            return self.text_response(step.text.as_deref().unwrap_or(&self.script.default_text));
        }
        let calls: Vec<ToolCall> = step
            .tool_calls
            .iter()
            .map(|c| ToolCall {
                id: format!("scripted-{}", Uuid::new_v4()),
                name: c.name.clone(),
                arguments: c.arguments.clone(),
            })
            .collect();
        let content = if calls.len() == 1 {
            MessageContent::ToolCall(calls.into_iter().next().expect("len checked"))
        } else {
            MessageContent::MultiToolCall(calls)
        };
        ModelResponse {
            message: assistant_message(content),
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            text: step.text.clone(),
        }
    }
}

fn assistant_message(content: MessageContent) -> Message {
    Message {
        id: Uuid::new_v4(),
        role: Role::Assistant,
        content,
        created_at: Utc::now(),
        // Unstamped, like the other providers' wire conversions: the
        // runner stamps the messages it keeps.
        agent_version: None,
    }
}

#[async_trait]
impl ModelProvider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted"
    }

    async fn chat(&self, messages: &[Message], _tools: &[ToolSchema]) -> Result<ModelResponse> {
        // Anchor on the most recent user message that matches a trigger,
        // then count the assistant turns after it — those are the steps
        // already replayed.
        let anchor = messages.iter().enumerate().rev().find_map(|(i, m)| {
            if m.role != Role::User {
                return None;
            }
            let text = m.content.as_text()?.to_lowercase();
            // Longest trigger wins, so one trigger being a prefix or
            // substring of another resolves to the more specific scenario
            // rather than whichever happens to be declared first.
            // Matching is case-insensitive: when a scenario is driven from
            // a real keyboard, iOS autocapitalizes the first letter and a
            // case-sensitive compare would silently miss.
            self.script
                .scenarios
                .iter()
                .filter(|s| text.contains(&s.trigger.to_lowercase()))
                .max_by_key(|s| s.trigger.len())
                .map(|s| (i, s))
        });
        let Some((idx, scenario)) = anchor else {
            return Ok(self.text_response(&self.script.default_text));
        };
        let assistant_turns_since = messages[idx + 1..]
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .count();

        match scenario.steps.get(assistant_turns_since) {
            Some(step) => {
                tracing::info!(
                    trigger = %scenario.trigger,
                    step = assistant_turns_since,
                    "scripted provider replaying step"
                );
                Ok(self.step_response(step))
            }
            None => Ok(self.text_response(&self.script.default_text)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> Message {
        Message {
            id: Uuid::new_v4(),
            role: Role::User,
            content: MessageContent::Text(text.to_string()),
            created_at: Utc::now(),
            agent_version: None,
        }
    }

    fn tool_result(call_id: &str) -> Message {
        Message {
            id: Uuid::new_v4(),
            role: Role::Tool,
            content: MessageContent::ToolResult(rustykrab_core::types::ToolResult {
                call_id: call_id.to_string(),
                output: serde_json::json!({"ok": true}),
                is_error: false,
                images: vec![],
            }),
            created_at: Utc::now(),
            agent_version: None,
        }
    }

    const SCRIPT: &str = r#"{
        "defaultText": "Done.",
        "scenarios": [
            {
                "trigger": "e2e: set token",
                "steps": [
                    { "toolCalls": [ { "name": "credential_write",
                                       "arguments": { "action": "set",
                                                      "name": "t",
                                                      "value": "v" } } ] },
                    { "text": "Token stored." }
                ]
            }
        ]
    }"#;

    #[tokio::test]
    async fn replays_tool_call_then_text_then_default() {
        let p = ScriptedProvider::from_json(SCRIPT).unwrap();
        let mut msgs = vec![user("please e2e: set token now")];

        let r1 = p.chat(&msgs, &[]).await.unwrap();
        assert_eq!(r1.stop_reason, StopReason::ToolUse);
        let calls = r1.message.content.tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "credential_write");

        let call_id = calls[0].id.clone();
        msgs.push(r1.message.clone());
        msgs.push(tool_result(&call_id));

        let r2 = p.chat(&msgs, &[]).await.unwrap();
        assert_eq!(r2.stop_reason, StopReason::EndTurn);
        assert_eq!(r2.message.content.as_text(), Some("Token stored."));

        msgs.push(r2.message.clone());
        let r3 = p.chat(&msgs, &[]).await.unwrap();
        assert_eq!(r3.stop_reason, StopReason::EndTurn);
        assert_eq!(r3.message.content.as_text(), Some("Done."));
    }

    /// Driving a scenario from the iOS app means typing on a keyboard that
    /// autocapitalizes: "e2e: set token" arrives as "E2e: set token".
    #[tokio::test]
    async fn trigger_matching_ignores_case() {
        let p = ScriptedProvider::from_json(SCRIPT).unwrap();
        let r = p.chat(&[user("E2E: Set Token now")], &[]).await.unwrap();
        assert_eq!(r.stop_reason, StopReason::ToolUse);
    }

    #[tokio::test]
    async fn unmatched_message_gets_default_text() {
        let p = ScriptedProvider::from_json(SCRIPT).unwrap();
        let msgs = vec![user("hello there")];
        let r = p.chat(&msgs, &[]).await.unwrap();
        assert_eq!(r.stop_reason, StopReason::EndTurn);
        assert_eq!(r.message.content.as_text(), Some("Done."));
    }

    #[tokio::test]
    async fn new_user_message_restarts_matching() {
        let p = ScriptedProvider::from_json(SCRIPT).unwrap();
        let mut msgs = vec![user("e2e: set token")];
        let r1 = p.chat(&msgs, &[]).await.unwrap();
        msgs.push(r1.message.clone());
        msgs.push(tool_result("x"));
        msgs.push(user("e2e: set token"));

        // A fresh user message resets the step counter to zero.
        let r2 = p.chat(&msgs, &[]).await.unwrap();
        assert_eq!(r2.stop_reason, StopReason::ToolUse);
    }

    /// The agent runner injects user-role nudges mid-run; they must not
    /// re-anchor the replay onto a non-matching message.
    #[tokio::test]
    async fn injected_user_nudge_does_not_reset_replay() {
        let p = ScriptedProvider::from_json(SCRIPT).unwrap();
        let mut msgs = vec![user("e2e: set token")];
        let r1 = p.chat(&msgs, &[]).await.unwrap();
        msgs.push(r1.message.clone());
        msgs.push(tool_result("x"));
        // The runner's own nudge, not a scenario trigger.
        msgs.push(user("You produced text but did not call `task_complete`."));

        let r2 = p.chat(&msgs, &[]).await.unwrap();
        assert_eq!(r2.message.content.as_text(), Some("Token stored."));
    }

    #[test]
    fn rejects_malformed_script() {
        assert!(ScriptedProvider::from_json("{ nope }").is_err());
        assert!(ScriptedProvider::from_json(r#"{"unknown": 1}"#).is_err());
    }

    #[test]
    fn rejects_empty_or_contentless_steps() {
        let no_steps = r#"{"scenarios":[{"trigger":"t","steps":[]}]}"#;
        assert!(ScriptedProvider::from_json(no_steps).is_err());
        let empty_step = r#"{"scenarios":[{"trigger":"t","steps":[{}]}]}"#;
        assert!(ScriptedProvider::from_json(empty_step).is_err());
    }

    /// One trigger being a substring of another must resolve to the more
    /// specific scenario, not to whichever is declared first.
    #[tokio::test]
    async fn longest_trigger_wins() {
        let script = r#"{
            "scenarios": [
                { "trigger": "deploy", "steps": [ { "text": "generic" } ] },
                { "trigger": "deploy to prod", "steps": [ { "text": "specific" } ] }
            ]
        }"#;
        let p = ScriptedProvider::from_json(script).unwrap();
        let r = p
            .chat(&[user("please deploy to prod now")], &[])
            .await
            .unwrap();
        assert_eq!(r.message.content.as_text(), Some("specific"));

        let r = p.chat(&[user("please deploy now")], &[]).await.unwrap();
        assert_eq!(r.message.content.as_text(), Some("generic"));
    }
}
