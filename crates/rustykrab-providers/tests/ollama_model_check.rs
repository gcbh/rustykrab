//! `OllamaProvider::check_model` against an in-process HTTP server: it asks
//! `/api/show` for the configured model, and a 404 means the server has no
//! such model. Nothing here loads a model.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use rustykrab_core::model::{ModelCheck, ModelProvider};
use rustykrab_providers::OllamaProvider;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Answers every request with `status`; keeps each request's head and body.
async fn mock(status: &'static str, body: &'static str) -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            while !String::from_utf8_lossy(&buf).contains('}') {
                match socket.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            }
            sink.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&buf).to_string());
            let reply = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(reply.as_bytes()).await;
        }
    });
    (addr, seen)
}

fn provider(addr: SocketAddr) -> OllamaProvider {
    OllamaProvider::new("nope:1b").with_base_url(format!("http://{addr}"))
}

#[tokio::test]
async fn a_404_from_api_show_is_a_missing_model() {
    let (addr, seen) = mock("404 Not Found", r#"{"error":"model 'nope:1b' not found"}"#).await;
    let check = provider(addr).check_model().await;
    let ModelCheck::Missing(why) = check else {
        panic!("expected Missing, got {check:?}");
    };
    assert!(why.contains("nope:1b"), "{why}");
    let requests = seen.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].starts_with("POST /api/show "),
        "{}",
        requests[0]
    );
    assert!(
        requests[0].contains(r#""model":"nope:1b""#),
        "{}",
        requests[0]
    );
}

#[tokio::test]
async fn a_model_the_server_has_is_available() {
    let (addr, _) = mock("200 OK", r#"{"capabilities":["completion"]}"#).await;
    assert_eq!(provider(addr).check_model().await, ModelCheck::Available);
}

#[tokio::test]
async fn any_other_answer_says_nothing_about_the_model() {
    let (addr, _) = mock("500 Internal Server Error", r#"{"error":"boom"}"#).await;
    assert_eq!(provider(addr).check_model().await, ModelCheck::Unknown);

    // Nothing listening at all.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let closed = listener.local_addr().unwrap();
    drop(listener);
    assert_eq!(provider(closed).check_model().await, ModelCheck::Unknown);
}
