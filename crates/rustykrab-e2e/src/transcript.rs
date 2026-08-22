//! What a scenario actually produced, read back out of the daemon.
//!
//! Everything here is read out of the daemon's own SQLite store, which is
//! the only place it exists. The REST API speaks an app-facing shape —
//! `{role, content}` with content flattened to a string — so tool calls,
//! tool arguments, tool results, and the whole compaction bookmark are
//! invisible over HTTP. Reading the store is not a shortcut around the
//! API; it is the only way to see what the run actually did, and it has
//! the side benefit that assertions check what the system persisted
//! rather than the harness's own bookkeeping.
//!
//! `conversations.data` holds the conversation minus its messages (where
//! the summary and bookmark live); `messages.data` holds one serialized
//! `Message` per row, in `idx` order.

use serde_json::Value;

/// One tool call, paired with the result that came back for it.
#[derive(Debug, Clone)]
pub struct ToolInvocation {
    pub tool: String,
    pub args: Value,
    /// The tool's output, matched by call id. `None` when the run ended
    /// before a result arrived.
    pub output: Option<Value>,
    pub failed: bool,
}

/// A conversation, parsed into the shape assertions need.
#[derive(Debug, Clone, Default)]
pub struct Transcript {
    /// The last assistant text turn — what a user would actually read.
    pub final_text: String,
    /// Every assistant text turn, oldest first.
    pub assistant_texts: Vec<String>,
    pub calls: Vec<ToolInvocation>,
    /// True once `compacted_through` is set.
    pub compacted: bool,
    pub compaction_generation: u64,
    /// The summary standing in for the folded-away history.
    pub summary: Option<String>,
    /// Messages retained in history but no longer sent to the model.
    pub folded_messages: usize,
    /// Messages still sent verbatim.
    pub live_messages: usize,
    pub total_messages: usize,
    pub duration_ms: u128,
    /// Set when the run itself failed rather than merely scoring badly.
    pub error: Option<String>,
}

impl Transcript {
    /// Read a conversation straight out of the daemon's store.
    pub fn from_store(db_path: &std::path::Path, conv_id: &str) -> anyhow::Result<Self> {
        let conn = rusqlite::Connection::open_with_flags(
            db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;

        let meta_raw: String = conn.query_row(
            "SELECT data FROM conversations WHERE id = ?1",
            [conv_id],
            |row| row.get(0),
        )?;
        let meta: Value = serde_json::from_str(&meta_raw)?;

        let mut stmt =
            conn.prepare("SELECT data FROM messages WHERE conversation_id = ?1 ORDER BY idx")?;
        let messages: Vec<Value> = stmt
            .query_map([conv_id], |row| row.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .filter_map(|raw| serde_json::from_str::<Value>(&raw).ok())
            .collect();

        Ok(Self::parse(&meta, &messages))
    }

    /// Assemble from the conversation metadata and its messages.
    pub fn parse(conv: &Value, messages: &[Value]) -> Self {
        let empty = Vec::new();

        let mut assistant_texts = Vec::new();
        let mut calls: Vec<ToolInvocation> = Vec::new();
        // call id -> index into `calls`, so a result can find its call even
        // when several tools ran in parallel and returned out of order.
        let mut by_call_id: Vec<(String, usize)> = Vec::new();

        for message in messages {
            let role = message["role"].as_str().unwrap_or_default();
            let kind = message["content"]["type"].as_str().unwrap_or_default();
            let data = &message["content"]["data"];

            match (role, kind) {
                ("assistant", "text") => {
                    if let Some(text) = data.as_str() {
                        let text = text.trim();
                        if !text.is_empty() {
                            assistant_texts.push(text.to_string());
                        }
                    }
                }
                // A channel that delivered an image alongside the reply
                // produces multi_part rather than text. Reading only the
                // text blocks keeps a picture from silently emptying the
                // answer an assertion is about to check.
                ("assistant", "multi_part") => {
                    let text = data
                        .as_array()
                        .unwrap_or(&empty)
                        .iter()
                        .filter_map(|block| block["text"].as_str())
                        .collect::<Vec<_>>()
                        .join(" ");
                    let text = text.trim();
                    if !text.is_empty() {
                        assistant_texts.push(text.to_string());
                    }
                }
                (_, "tool_call") => push_call(&mut calls, &mut by_call_id, data),
                (_, "multi_tool_call") => {
                    for call in data.as_array().unwrap_or(&empty) {
                        push_call(&mut calls, &mut by_call_id, call);
                    }
                }
                (_, "tool_result") => {
                    let call_id = data["call_id"].as_str().unwrap_or_default();
                    if let Some((_, idx)) = by_call_id.iter().find(|(id, _)| id == call_id) {
                        let invocation = &mut calls[*idx];
                        invocation.output = Some(data["output"].clone());
                        // A tool can also fail by returning an `error` key
                        // without the runner flagging it, which is how the
                        // agent loop itself decides a call went wrong.
                        invocation.failed = data["is_error"].as_bool().unwrap_or(false)
                            || data["output"].get("error").is_some();
                    }
                }
                _ => {}
            }
        }

        let folded_messages = match conv["compacted_through"].as_str() {
            Some(bookmark) => messages
                .iter()
                .position(|m| m["id"].as_str() == Some(bookmark))
                .map(|i| i + 1)
                .unwrap_or(0),
            None => 0,
        };

        Self {
            final_text: assistant_texts.last().cloned().unwrap_or_default(),
            assistant_texts,
            calls,
            compacted: !conv["compacted_through"].is_null(),
            compaction_generation: conv["compaction_generation"].as_u64().unwrap_or(0),
            summary: conv["summary"].as_str().map(str::to_string),
            folded_messages,
            live_messages: messages.len().saturating_sub(folded_messages),
            total_messages: messages.len(),
            duration_ms: 0,
            error: None,
        }
    }

    /// A run that never got off the ground. Every assertion fails against
    /// it, which is the honest outcome — an infrastructure failure is not
    /// a pass.
    pub fn failed(error: impl Into<String>, duration_ms: u128) -> Self {
        Self {
            error: Some(error.into()),
            duration_ms,
            ..Default::default()
        }
    }

    pub fn calls_to(&self, tool: &str) -> Vec<&ToolInvocation> {
        self.calls.iter().filter(|c| c.tool == tool).collect()
    }

    /// Every tool result for `tool`, rendered as text. Used to assert on
    /// what a tool actually returned to the model — memory recall, most
    /// importantly, where the question is whether retrieval found the fact
    /// at all, separately from whether the model then used it.
    pub fn outputs_of(&self, tool: &str) -> String {
        self.calls_to(tool)
            .iter()
            .filter_map(|c| c.output.as_ref())
            .map(|o| o.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn push_call(calls: &mut Vec<ToolInvocation>, by_call_id: &mut Vec<(String, usize)>, data: &Value) {
    if let Some(id) = data["id"].as_str() {
        by_call_id.push((id.to_string(), calls.len()));
    }
    calls.push(ToolInvocation {
        tool: data["name"].as_str().unwrap_or_default().to_string(),
        args: data["arguments"].clone(),
        output: None,
        failed: false,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Build the two halves the store keeps separately: conversation
    /// metadata, and the messages.
    fn conv(messages: Value, extra: Value) -> (Value, Vec<Value>) {
        let mut meta = json!({
            "id": "c1",
            "summary": null,
            "compacted_through": null,
            "compaction_generation": 0,
        });
        for (k, v) in extra.as_object().unwrap() {
            meta[k] = v.clone();
        }
        (meta, messages.as_array().unwrap().clone())
    }

    /// Test shim: the store hands these back separately.
    fn parse_pair(pair: &(Value, Vec<Value>)) -> Transcript {
        Transcript::parse(&pair.0, &pair.1)
    }

    #[test]
    fn pairs_tool_results_with_their_calls_by_id() {
        let t = parse_pair(&conv(
            json!([
                { "id": "m1", "role": "assistant",
                  "content": { "type": "tool_call",
                               "data": { "id": "call-1", "name": "weather",
                                         "arguments": { "city": "Oslo" } } } },
                { "id": "m2", "role": "tool",
                  "content": { "type": "tool_result",
                               "data": { "call_id": "call-1",
                                         "output": { "temperature_c": 3 },
                                         "is_error": false } } },
                { "id": "m3", "role": "assistant",
                  "content": { "type": "text", "data": "It is 3 degrees in Oslo." } }
            ]),
            json!({}),
        ));

        assert_eq!(t.calls.len(), 1);
        assert_eq!(t.calls[0].tool, "weather");
        assert_eq!(t.calls[0].args["city"], "Oslo");
        assert!(!t.calls[0].failed);
        assert!(t.outputs_of("weather").contains("temperature_c"));
        assert_eq!(t.final_text, "It is 3 degrees in Oslo.");
    }

    #[test]
    fn a_result_carrying_an_error_key_counts_as_a_failure() {
        // The agent loop treats an `error` key in the output as a failed
        // call even without is_error, so the transcript must agree — or a
        // recovery scenario would think nothing ever went wrong.
        let t = parse_pair(&conv(
            json!([
                { "id": "m1", "role": "assistant",
                  "content": { "type": "tool_call",
                               "data": { "id": "c", "name": "flaky", "arguments": {} } } },
                { "id": "m2", "role": "tool",
                  "content": { "type": "tool_result",
                               "data": { "call_id": "c", "output": { "error": "boom" } } } }
            ]),
            json!({}),
        ));
        assert!(t.calls[0].failed);
    }

    #[test]
    fn splits_parallel_tool_calls_into_separate_invocations() {
        let t = parse_pair(&conv(
            json!([
                { "id": "m1", "role": "assistant",
                  "content": { "type": "multi_tool_call",
                               "data": [
                                   { "id": "a", "name": "one", "arguments": {} },
                                   { "id": "b", "name": "two", "arguments": {} }
                               ] } }
            ]),
            json!({}),
        ));
        assert_eq!(t.calls.len(), 2);
        assert_eq!(t.calls[1].tool, "two");
    }

    #[test]
    fn reads_text_out_of_a_multi_part_reply() {
        let t = parse_pair(&conv(
            json!([
                { "id": "m1", "role": "assistant",
                  "content": { "type": "multi_part",
                               "data": [
                                   { "type": "image", "media_type": "image/png" },
                                   { "type": "text", "text": "the kettle is a Stagg" }
                               ] } }
            ]),
            json!({}),
        ));
        assert_eq!(t.final_text, "the kettle is a Stagg");
    }

    #[test]
    fn reads_the_compaction_bookmark() {
        let t = parse_pair(&conv(
            json!([
                { "id": "m1", "role": "user", "content": { "type": "text", "data": "old" } },
                { "id": "m2", "role": "assistant", "content": { "type": "text", "data": "older" } },
                { "id": "m3", "role": "user", "content": { "type": "text", "data": "new" } }
            ]),
            json!({ "compacted_through": "m2", "summary": "earlier chat",
                    "compaction_generation": 2 }),
        ));
        assert!(t.compacted);
        assert_eq!(t.folded_messages, 2);
        assert_eq!(t.live_messages, 1);
        assert_eq!(t.compaction_generation, 2);
        // History is hidden, never destroyed.
        assert_eq!(t.total_messages, 3);
    }
}
