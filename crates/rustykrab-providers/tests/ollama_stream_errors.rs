//! `OllamaProvider::chat_stream` against an in-process HTTP server that
//! streams NDJSON the way Ollama does. Once a stream has started, Ollama
//! reports a failure as a 200 whose last line is `{"error": …}`; it must be
//! classified as the same failure answered with a status would be. Nothing
//! here loads a model.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use rustykrab_core::model::{ModelProvider, ModelResponse, StopReason, StreamEvent};
use rustykrab_core::types::{Message, MessageContent, Role};
use rustykrab_core::{Error, Result};
use rustykrab_providers::OllamaProvider;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const PARTIAL_TEXT: &str =
    r#"{"model":"m","message":{"role":"assistant","content":"Writing the file."},"done":false}"#;
const TOOL_CALL: &str = r#"{"model":"m","message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"write","arguments":{"path":"a.py","content":"x = 1"}}}]},"done":false}"#;

/// Answers every `/api/chat` with a 200 streaming `lines` as chunked
/// NDJSON, one line per chunk; counts the requests.
async fn mock(lines: &'static [&'static str]) -> (SocketAddr, Arc<Mutex<usize>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(0));
    let count = requests.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            read_request(&mut socket).await;
            *count.lock().unwrap() += 1;
            let _ = socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\n\
                      Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .await;
            for line in lines {
                let line = format!("{line}\n");
                let chunk = format!("{:x}\r\n{line}\r\n", line.len());
                let _ = socket.write_all(chunk.as_bytes()).await;
            }
            let _ = socket.write_all(b"0\r\n\r\n").await;
        }
    });
    (addr, requests)
}

/// Read one request through the end of its body, so the reply is not sent
/// to a client still writing.
async fn read_request(socket: &mut TcpStream) {
    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = socket.read(&mut buf).await.unwrap();
        if n == 0 {
            return;
        }
        raw.extend_from_slice(&buf[..n]);
        let text = String::from_utf8_lossy(&raw);
        if let Some(split) = text.find("\r\n\r\n") {
            let length: usize = text[..split]
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse().unwrap())
                })
                .unwrap_or(0);
            if raw.len() >= split + 4 + length {
                return;
            }
        }
    }
}

/// One streamed call: its result, the text deltas it emitted, and whether
/// it reported `Done`.
async fn stream(addr: SocketAddr) -> (Result<ModelResponse>, Vec<String>, bool) {
    let provider = OllamaProvider::new("qwen3.8:27b").with_base_url(format!("http://{addr}"));
    let messages = [Message::stamped(
        Role::User,
        MessageContent::Text("write a.py".into()),
    )];
    let deltas = Mutex::new(Vec::new());
    let done = AtomicBool::new(false);
    let result = provider
        .chat_stream(&messages, &[], &|event| match event {
            StreamEvent::TextDelta(text) => deltas.lock().unwrap().push(text),
            StreamEvent::Done(_) => done.store(true, Ordering::SeqCst),
        })
        .await;
    (result, deltas.into_inner().unwrap(), done.into_inner())
}

/// The shape a qwen3.8 eval run died on: a reply under way, then the error
/// Ollama raises when it cannot parse the model's tool call. It used to be
/// read as a malformed chunk ("missing field `message`") and end the run.
#[tokio::test]
async fn a_tool_call_ollama_cannot_parse_mid_stream_is_malformed_output() {
    let (addr, requests) = mock(&[
        PARTIAL_TEXT,
        TOOL_CALL,
        r#"{"error":"XML syntax error on line 41: unexpected EOF"}"#,
    ])
    .await;
    let (result, deltas, done) = stream(addr).await;
    let err = result.expect_err("a cut-off reply is not a finished turn, tool call or not");
    assert!(
        matches!(err, Error::ModelMalformedOutput(ref m) if m.contains("line 41")),
        "{err:?}"
    );
    assert_eq!(
        deltas,
        ["Writing the file."],
        "text before the error streamed"
    );
    assert!(!done);
    assert_eq!(
        *requests.lock().unwrap(),
        1,
        "output the model must fix is not resent"
    );
}

#[tokio::test]
async fn a_runner_crash_mid_stream_stays_a_retryable_provider_error() {
    let (addr, requests) = mock(&[
        PARTIAL_TEXT,
        r#"{"error":"llama runner process has terminated: exit status 2"}"#,
    ])
    .await;
    let (result, _, done) = stream(addr).await;
    let err = result.expect_err("a crashed runner fails the call");
    assert!(
        matches!(err, Error::ModelProvider(ref m) if m.contains("runner process has terminated")),
        "{err:?}"
    );
    assert!(
        err.kind().retryable(),
        "the class a 500 with this body gets"
    );
    assert!(!done);
    assert_eq!(
        *requests.lock().unwrap(),
        1,
        "a stream that has started is not resent by the provider"
    );
}

#[tokio::test]
async fn a_stream_that_finishes_is_unchanged() {
    let (addr, requests) = mock(&[
        r#"{"model":"m","message":{"role":"assistant","content":"The answer"},"done":false}"#,
        r#"{"model":"m","message":{"role":"assistant","content":" is 4."},"done":false}"#,
        r#"{"model":"m","message":{"role":"assistant","content":""},"done":true,"done_reason":"stop","prompt_eval_count":12,"eval_count":5}"#,
    ])
    .await;
    let (result, deltas, done) = stream(addr).await;
    let response = result.expect("a complete stream is a reply");
    assert_eq!(response.message.content.as_text(), Some("The answer is 4."));
    assert_eq!(response.stop_reason, StopReason::EndTurn);
    assert_eq!(response.usage.prompt_tokens, 12);
    assert_eq!(response.usage.completion_tokens, 5);
    assert_eq!(deltas, ["The answer", " is 4."]);
    assert!(done);
    assert_eq!(*requests.lock().unwrap(), 1);
}
