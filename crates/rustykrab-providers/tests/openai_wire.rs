//! Wire-level tests for the OpenAI-compatible provider.
//!
//! These run the real provider against an in-process HTTP server so both
//! directions are checked against actual bytes: the request we emit, and the
//! response shapes real servers emit back (including SSE framing that splits
//! tool-call arguments across chunks).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustykrab_core::model::{ModelProvider, StopReason, StreamEvent};
use rustykrab_core::types::{Message, MessageContent, Role, ToolCall, ToolResult, ToolSchema};
use rustykrab_providers::OpenAiProvider;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use uuid::Uuid;

/// What the mock server should send back.
enum Reply {
    /// A complete JSON body with Content-Length.
    Json { status: u16, body: String },
    /// SSE frames written incrementally and delimited by connection close,
    /// so the provider's chunk loop sees genuinely partial reads.
    Sse(Vec<String>),
}

/// A captured inbound request.
#[derive(Clone)]
struct Captured {
    request_line: String,
    headers: String,
    body: serde_json::Value,
}

struct MockServer {
    addr: SocketAddr,
    captured: Arc<Mutex<Vec<Captured>>>,
}

impl MockServer {
    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn last(&self) -> Captured {
        self.captured
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("mock server received no request")
    }
}

async fn spawn_mock(reply: Reply) -> MockServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let sink = captured.clone();

    tokio::spawn(async move {
        let reply = reply;
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            if let Some(cap) = read_request(socket).await {
                let (socket, cap) = cap;
                sink.lock().unwrap().push(cap);
                write_reply(socket, &reply).await;
            }
        }
    });

    MockServer { addr, captured }
}

async fn read_request(mut socket: TcpStream) -> Option<(TcpStream, Captured)> {
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 4096];

    let head_end = loop {
        let n = socket.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
    };

    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let content_length = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);

    let body_start = head_end + 4;
    while buf.len() < body_start + content_length {
        let n = socket.read(&mut tmp).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }

    let raw_body = String::from_utf8_lossy(&buf[body_start..]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default().to_string();

    Some((
        socket,
        Captured {
            request_line,
            headers: head.clone(),
            body: serde_json::from_str(&raw_body).unwrap_or(serde_json::Value::Null),
        },
    ))
}

async fn write_reply(mut socket: TcpStream, reply: &Reply) {
    match reply {
        Reply::Json { status, body } => {
            let head = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(body.as_bytes()).await;
        }
        Reply::Sse(frames) => {
            // No Content-Length: the body is delimited by connection close,
            // which is what a streaming server does.
            let head =
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
            let _ = socket.write_all(head.as_bytes()).await;
            for frame in frames {
                let _ = socket.write_all(frame.as_bytes()).await;
                let _ = socket.flush().await;
                // Force the client to observe partial reads.
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    }
    let _ = socket.flush().await;
    let _ = socket.shutdown().await;
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

fn read_tool() -> ToolSchema {
    ToolSchema {
        name: "read".into(),
        description: "Read a file".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        }),
    }
}

// --- Request shape ---

#[tokio::test]
async fn posts_to_v1_chat_completions_with_expected_body() {
    let server = spawn_mock(Reply::Json {
        status: 200,
        body: serde_json::json!({
            "choices": [{"message": {"content": "hi"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 2}
        })
        .to_string(),
    })
    .await;

    let provider = OpenAiProvider::new("test-model").with_base_url(server.base_url());
    let resp = provider
        .chat(&[user("hello")], &[read_tool()])
        .await
        .unwrap();

    assert_eq!(resp.message.content.as_text(), Some("hi"));
    assert_eq!(resp.stop_reason, StopReason::EndTurn);
    assert_eq!(resp.usage.prompt_tokens, 5);

    let req = server.last();
    assert_eq!(req.request_line, "POST /v1/chat/completions HTTP/1.1");
    assert_eq!(req.body["model"], "test-model");
    assert_eq!(req.body["stream"], false);
    assert!(req.body.get("stream_options").is_none());
    // Tools must be advertised in OpenAI's function-wrapper shape.
    assert_eq!(req.body["tools"][0]["type"], "function");
    assert_eq!(req.body["tools"][0]["function"]["name"], "read");
    assert_eq!(
        req.body["tools"][0]["function"]["parameters"]["properties"]["path"]["type"],
        "string"
    );
}

#[tokio::test]
async fn tool_result_turn_serializes_with_tool_call_id() {
    let server = spawn_mock(Reply::Json {
        status: 200,
        body: serde_json::json!({"choices": [{"message": {"content": "done"}}]}).to_string(),
    })
    .await;

    let conversation = vec![
        user("read /tmp/x"),
        msg(
            Role::Assistant,
            MessageContent::ToolCall(ToolCall {
                id: "call_abc".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path": "/tmp/x"}),
            }),
        ),
        msg(
            Role::Tool,
            MessageContent::ToolResult(ToolResult {
                call_id: "call_abc".into(),
                output: serde_json::Value::String("file contents".into()),
                is_error: false,
                images: Vec::new(),
            }),
        ),
    ];

    let provider = OpenAiProvider::new("m").with_base_url(server.base_url());
    provider.chat(&conversation, &[read_tool()]).await.unwrap();

    let m = &server.last().body["messages"];
    assert_eq!(m[0]["role"], "user");

    // The assistant turn carries the call with its id, arguments JSON-encoded.
    assert_eq!(m[1]["role"], "assistant");
    assert_eq!(m[1]["tool_calls"][0]["id"], "call_abc");
    assert_eq!(m[1]["tool_calls"][0]["type"], "function");
    assert_eq!(
        m[1]["tool_calls"][0]["function"]["arguments"],
        r#"{"path":"/tmp/x"}"#
    );

    // The result references it — required by OpenAI, absent from Ollama's API.
    assert_eq!(m[2]["role"], "tool");
    assert_eq!(m[2]["tool_call_id"], "call_abc");
    assert_eq!(m[2]["content"], "file contents");
}

#[tokio::test]
async fn api_key_is_sent_as_bearer_and_omitted_when_unset() {
    let body = serde_json::json!({"choices": [{"message": {"content": "ok"}}]}).to_string();

    let with_key = spawn_mock(Reply::Json {
        status: 200,
        body: body.clone(),
    })
    .await;
    OpenAiProvider::new("m")
        .with_base_url(with_key.base_url())
        .with_api_key("sk-secret")
        .chat(&[user("hi")], &[])
        .await
        .unwrap();
    assert!(
        with_key
            .last()
            .headers
            .contains("authorization: Bearer sk-secret")
            || with_key
                .last()
                .headers
                .contains("Authorization: Bearer sk-secret")
    );

    let without = spawn_mock(Reply::Json { status: 200, body }).await;
    OpenAiProvider::new("m")
        .with_base_url(without.base_url())
        .chat(&[user("hi")], &[])
        .await
        .unwrap();
    assert!(!without
        .last()
        .headers
        .to_lowercase()
        .contains("authorization"));
}

// --- Response parsing ---

#[tokio::test]
async fn parses_parallel_tool_calls() {
    let server = spawn_mock(Reply::Json {
        status: 200,
        body: serde_json::json!({
            "choices": [{
                "message": {
                    "content": "",
                    "tool_calls": [
                        {"id": "c1", "type": "function",
                         "function": {"name": "read", "arguments": "{\"path\":\"/a\"}"}},
                        {"id": "c2", "type": "function",
                         "function": {"name": "read", "arguments": "{\"path\":\"/b\"}"}}
                    ]
                },
                "finish_reason": "tool_calls"
            }]
        })
        .to_string(),
    })
    .await;

    let resp = OpenAiProvider::new("m")
        .with_base_url(server.base_url())
        .chat(&[user("read both")], &[read_tool()])
        .await
        .unwrap();

    assert_eq!(resp.stop_reason, StopReason::ToolUse);
    let calls = resp.message.content.tool_calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].id, "c1");
    assert_eq!(calls[0].arguments, serde_json::json!({"path": "/a"}));
    assert_eq!(calls[1].arguments, serde_json::json!({"path": "/b"}));
}

#[tokio::test]
async fn http_errors_map_to_typed_variants() {
    for (status, expect) in [
        (400u16, "ModelBadRequest"),
        (401, "ModelAuthError"),
        (403, "ModelAuthError"),
    ] {
        let server = spawn_mock(Reply::Json {
            status,
            body: serde_json::json!({"error": {"message": "nope"}}).to_string(),
        })
        .await;

        let err = OpenAiProvider::new("m")
            .with_base_url(server.base_url())
            .chat(&[user("hi")], &[])
            .await
            .expect_err("expected an error");

        let variant = format!("{err:?}");
        assert!(
            variant.starts_with(expect),
            "status {status} produced {variant}, expected {expect}"
        );
        assert!(
            format!("{err}").contains("nope"),
            "error should carry the server body"
        );
    }
}

// --- Streaming ---

#[tokio::test]
async fn streams_text_deltas_and_emits_done() {
    let server = spawn_mock(Reply::Sse(vec![
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n".into(),
        "data: {\"choices\":[{\"delta\":{\"content\":\"lo \"}}]}\n\n".into(),
        "data: {\"choices\":[{\"delta\":{\"content\":\"world\"},\"finish_reason\":\"stop\"}]}\n\n"
            .into(),
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":3,\"prompt_tokens_details\":{\"cached_tokens\":8}}}\n\n".into(),
        "data: [DONE]\n\n".into(),
    ]))
    .await;

    let deltas = Arc::new(Mutex::new(Vec::new()));
    let done = Arc::new(Mutex::new(0usize));
    let (d, dn) = (deltas.clone(), done.clone());

    let resp = OpenAiProvider::new("m")
        .with_base_url(server.base_url())
        .chat_stream(&[user("hi")], &[], &move |ev| match ev {
            StreamEvent::TextDelta(t) => d.lock().unwrap().push(t),
            StreamEvent::Done(_) => *dn.lock().unwrap() += 1,
        })
        .await
        .unwrap();

    assert_eq!(*deltas.lock().unwrap(), vec!["Hel", "lo ", "world"]);
    assert_eq!(*done.lock().unwrap(), 1);
    assert_eq!(resp.message.content.as_text(), Some("Hello world"));
    assert_eq!(resp.stop_reason, StopReason::EndTurn);
    // Usage arrives in the trailer chunk after choices are exhausted.
    assert_eq!(resp.usage.prompt_tokens, 11);
    assert_eq!(resp.usage.cache_read_tokens, 8);

    // stream_options must be requested for that trailer to exist.
    assert_eq!(server.last().body["stream_options"]["include_usage"], true);
    assert_eq!(server.last().body["stream"], true);
}

#[tokio::test]
async fn reassembles_tool_call_arguments_split_across_frames() {
    // Argument fragments are not individually valid JSON — the provider must
    // concatenate them per index before parsing.
    let server = spawn_mock(Reply::Sse(vec![
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read\",\"arguments\":\"\"}}]}}]}\n\n".into(),
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"pa\"}}]}}]}\n\n".into(),
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"th\\\":\\\"/tmp\"}}]}}]}\n\n".into(),
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"/x.txt\\\"}\"}}]}}]}\n\n".into(),
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n".into(),
        "data: [DONE]\n\n".into(),
    ]))
    .await;

    let resp = OpenAiProvider::new("m")
        .with_base_url(server.base_url())
        .chat_stream(&[user("read it")], &[read_tool()], &|_| {})
        .await
        .unwrap();

    assert_eq!(resp.stop_reason, StopReason::ToolUse);
    let calls = resp.message.content.tool_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "call_1");
    assert_eq!(calls[0].name, "read");
    assert_eq!(
        calls[0].arguments,
        serde_json::json!({"path": "/tmp/x.txt"}),
        "fragmented arguments must reassemble into the original object"
    );
}

#[tokio::test]
async fn streams_parallel_tool_calls_interleaved_by_index() {
    // Real servers interleave fragments for concurrent calls.
    let server = spawn_mock(Reply::Sse(vec![
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\"\"}}]}}]}\n\n".into(),
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"c2\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\"\"}}]}}]}\n\n".into(),
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\":\\\"/a\\\"}\"}}]}}]}\n\n".into(),
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"function\":{\"arguments\":\":\\\"/b\\\"}\"}}]}}]}\n\n".into(),
        "data: [DONE]\n\n".into(),
    ]))
    .await;

    let resp = OpenAiProvider::new("m")
        .with_base_url(server.base_url())
        .chat_stream(&[user("read both")], &[read_tool()], &|_| {})
        .await
        .unwrap();

    let calls = resp.message.content.tool_calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].id, "c1");
    assert_eq!(calls[0].arguments, serde_json::json!({"path": "/a"}));
    assert_eq!(calls[1].id, "c2");
    assert_eq!(calls[1].arguments, serde_json::json!({"path": "/b"}));
}

#[tokio::test]
async fn tolerates_sse_comments_and_frames_split_mid_line() {
    // A frame deliberately split so the provider must buffer across reads.
    let server = spawn_mock(Reply::Sse(vec![
        ": keep-alive\n\n".into(),
        "data: {\"choices\":[{\"delta\":{\"cont".into(),
        "ent\":\"split\"}}]}\n\n".into(),
        "\n".into(),
        "data: [DONE]\n\n".into(),
    ]))
    .await;

    let resp = OpenAiProvider::new("m")
        .with_base_url(server.base_url())
        .chat_stream(&[user("hi")], &[], &|_| {})
        .await
        .unwrap();

    assert_eq!(resp.message.content.as_text(), Some("split"));
}

#[tokio::test]
async fn zero_argument_tool_call_yields_empty_object() {
    let server = spawn_mock(Reply::Sse(vec![
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"now\",\"arguments\":\"\"}}]}}]}\n\n".into(),
        "data: [DONE]\n\n".into(),
    ]))
    .await;

    let resp = OpenAiProvider::new("m")
        .with_base_url(server.base_url())
        .chat_stream(&[user("time?")], &[], &|_| {})
        .await
        .unwrap();

    let calls = resp.message.content.tool_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].arguments, serde_json::json!({}));
}
