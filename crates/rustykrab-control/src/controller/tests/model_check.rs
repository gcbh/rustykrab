//! A local worker whose model is missing is unhealthy and takes no lease.
//! The worker here asks a mock Ollama `/api/show` over real HTTP the way
//! `LocalWorker` does through its provider's `check_model`: a 404 means the
//! server has no such model. Registration asks once, and a registry refresh
//! asks again, so a model pulled later makes the worker leasable.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use rustykrab_core::work::{ResultReport, Status, WorkerKind};
use rustykrab_core::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::{done, draft, Harness};
use crate::controller::{Controller, ControllerConfig};
use crate::registry::WorkerRegistry;
use crate::worker::{Brief, Worker, WorkerCapabilities};

/// A mock Ollama that answers every `/api/show` with `status`.
struct MockOllama {
    addr: SocketAddr,
    status: Arc<AtomicU16>,
}

async fn mock_ollama(status: u16) -> MockOllama {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let status = Arc::new(AtomicU16::new(status));
    let answer = status.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            // The request's head and its short JSON body.
            while !String::from_utf8_lossy(&buf).contains("}") {
                match socket.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            }
            let (line, body) = match answer.load(Ordering::SeqCst) {
                200 => ("200 OK", r#"{"capabilities":["completion"]}"#),
                404 => ("404 Not Found", r#"{"error":"model 'nope:1b' not found"}"#),
                _ => ("500 Internal Server Error", r#"{"error":"boom"}"#),
            };
            let reply = format!(
                "HTTP/1.1 {line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(reply.as_bytes()).await;
        }
    });
    MockOllama { addr, status }
}

/// The status line's code of one `POST /api/show` for `model`.
async fn api_show(addr: SocketAddr, model: &str) -> Option<u16> {
    let mut socket = TcpStream::connect(addr).await.ok()?;
    let body = format!(r#"{{"model":"{model}"}}"#);
    let request = format!(
        "POST /api/show HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(request.as_bytes()).await.ok()?;
    let mut reply = String::new();
    socket.read_to_string(&mut reply).await.ok()?;
    reply.split_whitespace().nth(1)?.parse().ok()
}

/// A local worker on a model served by a [`MockOllama`]: unhealthy once
/// the server has answered 404, as `LocalWorker` is.
struct Snapper {
    server: SocketAddr,
    missing: AtomicBool,
}

#[async_trait]
impl Worker for Snapper {
    fn name(&self) -> &str {
        "snapper"
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::Local
    }
    fn capabilities(&self) -> WorkerCapabilities {
        WorkerCapabilities {
            models: vec!["ollama".to_string()],
            ..WorkerCapabilities::default()
        }
    }
    fn healthy(&self) -> bool {
        !self.missing.load(Ordering::SeqCst)
    }
    async fn run(&self, brief: Brief) -> Result<ResultReport, Error> {
        Ok(done(&brief.title))
    }
    async fn refresh(&self) -> bool {
        // Unknown (no answer, a 500) keeps what the worker had.
        match api_show(self.server, "nope:1b").await {
            Some(404) => self.missing.store(true, Ordering::SeqCst),
            Some(s) if (200..300).contains(&s) => self.missing.store(false, Ordering::SeqCst),
            _ => {}
        }
        true
    }
}

#[tokio::test]
async fn a_local_worker_whose_model_is_missing_is_unhealthy_and_leased_nothing() {
    let server = mock_ollama(404).await;
    let mut h = Harness::new(&[]);
    let registry = Arc::new(WorkerRegistry::new(h.store().clone()));
    let snapper = Arc::new(Snapper {
        server: server.addr,
        missing: AtomicBool::new(false),
    });

    // Registration asks the server, so the row is unhealthy from the start.
    let view = registry
        .register(snapper.clone(), serde_json::json!({}), None)
        .await
        .unwrap();
    assert!(
        !view.healthy,
        "a 404 from /api/show makes the worker unhealthy"
    );
    assert_eq!(view.health, "unhealthy");
    let listed = registry.view("snapper").await.unwrap().unwrap();
    assert!(listed.live && !listed.healthy, "{listed:?}");

    h.ctl = Controller::new(h.store().clone(), Vec::new(), ControllerConfig::default())
        .with_clock(h.clock.clone())
        .with_registry(registry.clone());
    let id = h.file_one(draft("a", "sort the shells")).await;
    h.drain().await;
    assert_eq!(h.leases(&id).await, 0, "nothing is leased to snapper");
    assert_ne!(h.status(&id).await, Status::Done);

    // The model is pulled: the next refresh records it and the item runs.
    server.status.store(200, Ordering::SeqCst);
    assert_eq!(registry.refresh().await.unwrap(), ["snapper"]);
    assert!(registry.view("snapper").await.unwrap().unwrap().healthy);
    h.drain().await;
    assert_eq!(h.leases(&id).await, 1);
    assert_eq!(h.status(&id).await, Status::Done);
}

#[tokio::test]
async fn a_server_that_cannot_say_leaves_the_local_worker_as_it_was() {
    let server = mock_ollama(500).await;
    let h = Harness::new(&[]);
    let registry = WorkerRegistry::new(h.store().clone());
    let snapper = Arc::new(Snapper {
        server: server.addr,
        missing: AtomicBool::new(false),
    });
    let view = registry
        .register(snapper, serde_json::json!({}), None)
        .await
        .unwrap();
    assert!(view.healthy, "a 500 says nothing about the model");
}
