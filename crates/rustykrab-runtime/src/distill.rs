//! Deciding what a message is worth remembering.
//!
//! Auto-persist writes every turn that reaches it into working memory, which
//! decays. That tier is deliberately indiscriminate and stays that way: it is
//! the recall buffer, and dropping from it is how the lifecycle sweep is meant
//! to work.
//!
//! What did not exist is a *durable* tier populated on purpose. The regex
//! extractor was standing in for one and could not do the job: measured on the
//! live store, its single most frequent output was `Your scheduled task / is /
//! due again`, 61 times, while the user's own name and phone number — supplied
//! in chat — appear zero times. The `key_value` rule that produced that noise
//! is `(?i)\b([A-Z][a-z]+...)`, whose `(?i)` defeats its own capitalisation
//! guard, so it matches "any words + is + any words".
//!
//! So this module asks a model instead, once per inbound message, and writes
//! what comes back at [`LifecycleStage::Semantic`].
//!
//! ## Two things that are easy to get wrong
//!
//! **Statements, not triples.** A fact is stored as a sentence — "The user's
//! full name is Geoffrey Heath" — because the retrieval path is a vector
//! search over memory *content*. `extracted_facts` rows are never embedded
//! (`store_facts` is a bare INSERT), so a triple would be reachable only
//! through the embedding of the message it came from. That message here was
//! "Book me at the earliest time. Geoffrey Heath \n4259418019", which embeds
//! as a booking request; a query like "what is the user's phone number" does
//! not match it. Writing the sentence puts an identity-shaped string in the
//! index, which is what identity-shaped queries will actually find.
//!
//! **Fail closed, safely.** An unparseable or failed classification writes
//! nothing to the durable tier. That is only safe because the raw turn has
//! already gone to working memory on the same path — a classifier outage
//! costs a distillation, never a message.

use std::sync::Arc;

use chrono::Utc;
use rustykrab_core::types::{Conversation, Message, MessageContent, Role};
use rustykrab_core::ModelProvider;

use crate::context::AgentContext;
use rustykrab_memory::types::{ConversationTurn, LifecycleStage, TurnMetadata};
use rustykrab_memory::MemorySystem;
use uuid::Uuid;

/// Most statements one message may contribute.
///
/// A bound rather than a belief: it caps the damage when a model decides to
/// enumerate rather than distil.
const MAX_STATEMENTS: usize = 5;

/// Longest statement kept, in characters.
const MAX_STATEMENT_CHARS: usize = 240;

/// What the classifier is asked to do.
///
/// Durability rather than importance is the test, because models over-report
/// importance and under-report durability. The negative examples are the
/// message classes measured to dominate this store: task chatter, tool
/// errors, and the runner's own nudges.
///
/// The recurrence clause is load-bearing and was added on evidence. An
/// earlier wording listed "standing instructions" as durable and also listed
/// "anything about what the assistant should do right now" as not, and the
/// second consistently won: gemma4:26b returned nothing for "check my email
/// every day at 7am and 5:30pm" — the very instruction that configures the
/// daily briefing. Naming the contrast explicitly fixed it. Measured against
/// the live model, this wording scores 6/6 on identity, standing
/// instructions, one-off requests, transient asks, assistant chatter, and a
/// stated preference, in 0.6–1.2s per message.
fn system_prompt() -> &'static str {
    "You extract durable facts about the user from one chat message.\n\
     Durable: identity, contact details, addresses, relationships, stable \
     preferences, and standing instructions that recur (\"every day\", \
     \"always\", \"from now on\", a schedule).\n\
     Not durable: a one-off request for something to happen now, questions, \
     errors, or status updates.\n\
     A request that repeats is durable even though it is phrased as an \
     instruction: \"check my email every morning\" is a standing \
     instruction; \"check my email\" is not.\n\
     Write each fact as one self-contained sentence naming the user \
     explicitly, e.g. \"The user's phone number is 555-0100.\" — never a \
     pronoun, never a fragment.\n\
     Reply with JSON only, no prose and no code fence: \
     {\"facts\": [\"...\"]}. If there is nothing durable, reply {\"facts\": []}."
}

/// Pull the statement list out of a classifier reply.
///
/// `None` means the reply could not be trusted and the caller must write
/// nothing. Distinct from `Some(vec![])`, which is the model correctly saying
/// this message carried nothing durable — a common and healthy answer.
///
/// Tolerates a code fence because local models add one regardless of
/// instructions; anything beyond that is treated as a failed classification
/// rather than something to repair by guesswork.
pub(crate) fn parse_statements(raw: &str) -> Option<Vec<String>> {
    let trimmed = raw.trim();
    // Strip a ```json ... ``` wrapper if present.
    let body = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .map(|s| s.trim_start().trim_end_matches("```").trim())
        .unwrap_or(trimmed);

    let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
    let facts = parsed.get("facts")?.as_array()?;

    let mut out = Vec::new();
    for f in facts.iter().take(MAX_STATEMENTS) {
        let s = f.as_str()?.trim();
        if s.is_empty() {
            continue;
        }
        let mut s = s.to_string();
        if s.chars().count() > MAX_STATEMENT_CHARS {
            s = s.chars().take(MAX_STATEMENT_CHARS).collect();
        }
        out.push(s);
    }
    Some(out)
}

/// Whether this message is worth spending a classification call on.
///
/// User text only. Assistant and tool output is where the measured noise
/// comes from, and the runner's synthesised prompts ("Continue from the
/// summary above", "You produced text but did not call `task_complete`")
/// arrive with `Role::User` too — they are the single largest contributor to
/// the existing store and carry nothing about the person.
pub(crate) fn worth_classifying(msg: &Message) -> bool {
    if msg.role != Role::User {
        return false;
    }
    let MessageContent::Text(text) = &msg.content else {
        return false;
    };
    let t = text.trim();
    if t.is_empty() {
        return false;
    }
    !RUNNER_PROMPT_PREFIXES.iter().any(|p| t.starts_with(p))
}

/// Openings of the prompts the runner writes as if it were the user.
const RUNNER_PROMPT_PREFIXES: &[&str] = &[
    "Continue from the summary above",
    "You produced text but did not call",
    "You have reached the iteration limit",
    "Multiple consecutive tool calls have failed",
];

/// A one-off message for the classifier exchange.
fn text_message(role: Role, text: String) -> Message {
    Message {
        id: Uuid::new_v4(),
        role,
        content: MessageContent::Text(text),
        created_at: Utc::now(),
        agent_version: Message::version_stamp(),
    }
}

/// Ask the classifier what is durable in `content`.
///
/// Returns `None` on any failure — transport, refusal, or unparseable reply.
async fn distil(provider: &Arc<dyn ModelProvider>, content: &str) -> Option<Vec<String>> {
    let messages = vec![
        text_message(Role::System, system_prompt().to_string()),
        text_message(Role::User, format!("Message: {content:?}")),
    ];
    let response = match provider.chat(&messages, &[]).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "distillation call failed — nothing written");
            return None;
        }
    };
    let text = response.message.content.as_text().unwrap_or("");
    let parsed = parse_statements(text);
    if parsed.is_none() {
        tracing::warn!(
            reply = %text.chars().take(120).collect::<String>(),
            "distillation reply was unparseable — nothing written"
        );
    }
    parsed
}

/// Classify `msg` and write any durable statements to semantic memory.
///
/// Spawned detached by the caller: there is one local model and agent turns
/// have been measured taking minutes, so anything on the inbound path must
/// not wait for it. A statement landing late is indistinguishable from one
/// landing instantly; a message blocked behind a busy model is not.
pub async fn distil_into_memory(
    provider: Arc<dyn ModelProvider>,
    memory: Arc<MemorySystem>,
    agent_id: Uuid,
    session_id: Uuid,
    content: String,
) {
    let Some(statements) = distil(&provider, &content).await else {
        return;
    };
    if statements.is_empty() {
        return;
    }
    for (i, statement) in statements.into_iter().enumerate() {
        let turn = ConversationTurn {
            id: Uuid::new_v4(),
            session_id,
            // Distillates are not turns in the conversation; the number only
            // has to be stable and non-colliding within this batch.
            turn_number: i as u32,
            // Attributed to the user because that is who the statement is
            // about, which is also how it scopes for retrieval.
            speaker: "user".to_string(),
            content: statement.clone(),
            token_count: Some(rustykrab_core::estimate_message_bytes(statement.len()) as u32),
            metadata: TurnMetadata {
                involves_tool_use: false,
                user_flagged: false,
                tags: vec!["distilled".to_string()],
            },
        };
        match memory
            .retain_with_stage(turn, agent_id, LifecycleStage::Semantic)
            .await
        {
            Ok(_) => tracing::info!(statement = %statement, "distilled a durable fact"),
            Err(e) => tracing::warn!(error = %e, "failed to store a distilled fact"),
        }
    }
}

/// Take an inbound message into memory: the raw turn always, a distillation
/// when the classifier finds one.
///
/// Channels append the inbound message straight onto `conv.messages`, which
/// never passes through `AgentRunner::push_message` and so never reaches the
/// auto-persist hook. The measurable consequence: of everything the user has
/// ever typed into Telegram, none of it is in memory, while the runner's own
/// synthesised prompts are there repeatedly. This is the call that closes
/// that gap, and it is deliberately the same call that starts distillation —
/// two behaviours that must not drift apart, since the second is worthless
/// without the first.
pub async fn ingest_inbound(ctx: &AgentContext, conv: &Conversation, msg: &Message) {
    let (Some(memory), Some(agent_id)) = (ctx.memory.clone(), ctx.agent_id) else {
        return;
    };

    // The raw turn, unconditionally. `turn_number` mirrors the auto-persist
    // callback's convention: position in the conversation as it stands.
    let turn_number = conv.messages.len().saturating_sub(1) as u32;
    let turn = crate::orchestrate::message_to_turn(msg, conv.id, turn_number);
    if let Err(e) = memory
        .retain_with_stage(turn, agent_id, LifecycleStage::Working)
        .await
    {
        tracing::warn!(error = %e, "failed to persist inbound turn to working memory");
    }

    // The distillation, if there is a classifier and the message is worth
    // spending one on. Detached: nothing on the inbound path waits for a
    // model that may be busy with an agent turn.
    let Some(distiller) = ctx.distiller.clone() else {
        return;
    };
    if !worth_classifying(msg) {
        return;
    }
    let MessageContent::Text(content) = &msg.content else {
        return;
    };
    let content = content.clone();
    let session_id = conv.id;
    tokio::spawn(async move {
        distil_into_memory(distiller, memory, agent_id, session_id, content).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_reply_parses() {
        let got = parse_statements(r#"{"facts": ["The user's name is Ada."]}"#).unwrap();
        assert_eq!(got, vec!["The user's name is Ada.".to_string()]);
    }

    #[test]
    fn an_empty_list_is_success_not_failure() {
        // The difference that matters: the model saying "nothing here" must
        // not look like the model failing, or a quiet outage would read as a
        // stream of unremarkable messages.
        assert_eq!(parse_statements(r#"{"facts": []}"#), Some(vec![]));
    }

    #[test]
    fn a_code_fence_is_tolerated() {
        let raw = "```json\n{\"facts\": [\"The user lives in Lisbon.\"]}\n```";
        assert_eq!(
            parse_statements(raw),
            Some(vec!["The user lives in Lisbon.".to_string()])
        );
    }

    #[test]
    fn garbage_fails_closed() {
        // Every one of these must be None, not an empty list: the caller
        // writes nothing either way, but only None is a failure worth logging.
        for raw in [
            "I'm not sure what you mean.",
            "{\"facts\": \"not an array\"}",
            "{\"other\": []}",
            "",
        ] {
            assert_eq!(parse_statements(raw), None, "should not parse: {raw:?}");
        }
    }

    #[test]
    fn a_runaway_list_is_capped() {
        let many: Vec<String> = (0..50).map(|i| format!("Fact {i}")).collect();
        let raw = serde_json::json!({ "facts": many }).to_string();
        assert_eq!(parse_statements(&raw).unwrap().len(), MAX_STATEMENTS);
    }

    #[test]
    fn an_overlong_statement_is_truncated_not_dropped() {
        let long = "x".repeat(MAX_STATEMENT_CHARS + 50);
        let raw = serde_json::json!({ "facts": [long] }).to_string();
        let got = parse_statements(&raw).unwrap();
        assert_eq!(got[0].chars().count(), MAX_STATEMENT_CHARS);
    }

    #[test]
    fn the_runners_own_prompts_are_not_classified() {
        // These arrive as Role::User and are the largest single contributor
        // to the existing store: 144 memories begin with the task list, 26
        // with the task_complete nudge. Spending a model call on them is
        // pure waste, and the model has nothing to find.
        for text in [
            "Continue from the summary above. Do not repeat already-completed work.",
            "You produced text but did not call `task_complete`.",
            "You have reached the iteration limit (25 iterations).",
        ] {
            let msg = text_message(Role::User, text.to_string());
            assert!(!worth_classifying(&msg), "should be skipped: {text:?}");
        }
    }

    #[test]
    fn a_real_user_message_is_classified() {
        let msg = text_message(
            Role::User,
            "Book me at the earliest time. Geoffrey Heath".to_string(),
        );
        assert!(worth_classifying(&msg));
    }

    #[test]
    fn assistant_output_is_not_classified() {
        // The measured noise is all assistant-side: "Your scheduled task is
        // due again" was extracted 61 times from it.
        let msg = text_message(
            Role::Assistant,
            "Your scheduled task is due again.".to_string(),
        );
        assert!(!worth_classifying(&msg));
    }
}
