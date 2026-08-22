//! What a scenario actually produced, read back out of the daemon.
//!
//! Everything here comes from `GET /api/conversations/{id}` — the same
//! record the daemon persisted. That is deliberate: an in-process harness
//! can watch tool calls through a side channel, but then it is asserting
//! on its own bookkeeping rather than on what the system stored. Tool
//! calls, tool results, the compaction bookmark, and the summary are all
//! in the persisted conversation, so the assertions read the real thing.

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
    /// Parse a conversation as returned by the REST API.
    pub fn parse(conv: &Value) -> Self {
        let empty = Vec::new();
        let messages = conv["messages"].as_array().unwrap_or(&empty);

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

        // A compacted conversation is one carrying a summary: `summary` is
        // the only compaction state a `Conversation` actually holds. There
        // is no bookmark field and no generation counter, so how much was
        // folded away cannot be read back — only that it happened.
        let compacted = conv["summary"].as_str().is_some_and(|s| !s.is_empty());

        Self {
            final_text: assistant_texts.last().cloned().unwrap_or_default(),
            assistant_texts,
            calls,
            compacted,
            compaction_generation: u64::from(compacted),
            summary: conv["summary"].as_str().map(str::to_string),
            folded_messages: 0,
            live_messages: messages.len(),
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

    fn conv(messages: Value, extra: Value) -> Value {
        let mut c = json!({
            "id": "c1",
            "messages": messages,
            "summary": null,
            "compacted_through": null,
            "compaction_generation": 0,
        });
        for (k, v) in extra.as_object().unwrap() {
            c[k] = v.clone();
        }
        c
    }

    #[test]
    fn pairs_tool_results_with_their_calls_by_id() {
        let t = Transcript::parse(&conv(
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
        let t = Transcript::parse(&conv(
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
        let t = Transcript::parse(&conv(
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
        let t = Transcript::parse(&conv(
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
    fn reads_compaction_state_from_the_summary() {
        let messages = json!([
            { "id": "m1", "role": "user", "content": { "type": "text", "data": "old" } },
            { "id": "m2", "role": "assistant", "content": { "type": "text", "data": "older" } },
            { "id": "m3", "role": "user", "content": { "type": "text", "data": "new" } }
        ]);

        // A summary is the whole of the compaction state a `Conversation`
        // carries — there is no bookmark and no generation counter to read.
        let t = Transcript::parse(&conv(
            messages.clone(),
            json!({ "summary": "earlier chat" }),
        ));
        assert!(t.compacted);
        assert_eq!(t.compaction_generation, 1);
        assert_eq!(t.summary.as_deref(), Some("earlier chat"));
        // History is hidden, never destroyed.
        assert_eq!(t.total_messages, 3);
        assert_eq!(t.live_messages, 3);

        let t = Transcript::parse(&conv(messages, json!({ "summary": null })));
        assert!(!t.compacted);
        assert_eq!(t.compaction_generation, 0);
    }
}
