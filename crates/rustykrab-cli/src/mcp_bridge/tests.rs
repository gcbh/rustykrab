use super::*;
use axum::extract::State;
use axum::http::HeaderMap as Headers;
use axum::response::IntoResponse;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, DuplexStream, Lines};

const GRANT: &str = "rkmcp_0123456789abcdef0123456789abcdef0123456789abcdef0123456789ab";

/// What the stand-in daemon saw.
#[derive(Default)]
struct Seen {
    headers: Mutex<Vec<Headers>>,
    calls: AtomicUsize,
    calls_in_flight: AtomicUsize,
    most_calls_in_flight: AtomicUsize,
    redirected: AtomicUsize,
}

/// A stand-in for the daemon's MCP route. `leak` is refused with a body
/// that repeats the presented credential, `echo` answers with it,
/// `redirect` points elsewhere, a `tools/call` takes a moment, a
/// notification is accepted.
async fn stand_in(
    State(seen): State<Arc<Seen>>,
    headers: Headers,
    body: String,
) -> axum::response::Response {
    seen.headers.lock().unwrap().push(headers.clone());
    let message: Value = serde_json::from_str(&body).unwrap();
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let id = message["id"].clone();
    match message["method"].as_str() {
        _ if id.is_null() => StatusCode::ACCEPTED.into_response(),
        Some("leak") => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("bad credential {auth}"),
        )
            .into_response(),
        Some("echo") => axum::Json(json!({"jsonrpc":"2.0","id":id,"result":{"credential":auth}}))
            .into_response(),
        Some("redirect") => axum::response::Redirect::temporary("/elsewhere").into_response(),
        Some("tools/call") => {
            seen.calls.fetch_add(1, Ordering::SeqCst);
            let now = seen.calls_in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            seen.most_calls_in_flight.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(200)).await;
            seen.calls_in_flight.fetch_sub(1, Ordering::SeqCst);
            axum::Json(json!({"jsonrpc":"2.0","id":id,"result":{"content":[]}})).into_response()
        }
        _ => axum::Json(json!({"jsonrpc":"2.0","id":id,"result":{}})).into_response(),
    }
}

async fn daemon() -> (Url, Arc<Seen>) {
    let seen = Arc::new(Seen::default());
    let app = axum::Router::new()
        .route(PATH, axum::routing::post(stand_in))
        .route(
            "/elsewhere",
            axum::routing::post(|State(seen): State<Arc<Seen>>| async move {
                seen.redirected.fetch_add(1, Ordering::SeqCst);
                StatusCode::OK
            }),
        )
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, seen)
}

/// A log the test can read back.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

/// The agent's ends of a bridge serving on in-memory pipes.
struct Agent {
    input: DuplexStream,
    output: Lines<BufReader<DuplexStream>>,
    bridge: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Agent {
    fn start(base: &Url, log: Captured) -> Agent {
        let bridge = Bridge::new(base, GRANT, Box::new(log)).unwrap();
        let (input, bridge_input) = tokio::io::duplex(1 << 16);
        let (bridge_output, output) = tokio::io::duplex(1 << 16);
        Agent {
            input,
            output: BufReader::new(output).lines(),
            bridge: tokio::spawn(bridge.serve(bridge_input, bridge_output)),
        }
    }

    async fn send(&mut self, message: Value) {
        self.send_raw(format!("{message}\n").as_bytes()).await;
    }

    async fn send_raw(&mut self, bytes: &[u8]) {
        self.input.write_all(bytes).await.unwrap();
    }

    async fn reply(&mut self) -> (String, Value) {
        let line = tokio::time::timeout(Duration::from_secs(10), self.output.next_line())
            .await
            .expect("a reply")
            .unwrap()
            .expect("an open stream");
        let value = serde_json::from_str(&line).unwrap();
        (line, value)
    }

    async fn close(self) {
        drop(self.input);
        self.bridge.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn the_grant_reaches_the_daemon_and_nothing_the_bridge_writes() {
    let (base, seen) = daemon().await;
    let log = Captured::default();
    let mut agent = Agent::start(&base, log.clone());
    agent
        .send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
        .await;
    agent
        .send(json!({"jsonrpc":"2.0","id":1,"method":"leak"}))
        .await;
    let (line, refused) = agent.reply().await;
    assert_eq!(refused["id"], 1);
    assert_eq!(refused["error"]["code"], SERVER_ERROR);
    assert!(
        line.contains("[redacted]") && !line.contains(GRANT),
        "{line}"
    );
    agent
        .send(json!({"jsonrpc":"2.0","id":"two","method":"echo"}))
        .await;
    let (line, echoed) = agent.reply().await;
    assert_eq!(echoed["id"], "two");
    assert!(
        line.contains("[redacted]") && !line.contains(GRANT),
        "{line}"
    );
    agent.close().await;

    // It was presented to the daemon, as a bearer, with the gateway's own
    // origin and one session for every message, a notification included.
    let headers = seen.headers.lock().unwrap().clone();
    assert_eq!(headers.len(), 3);
    for sent in headers.iter() {
        assert_eq!(sent["authorization"], format!("Bearer {GRANT}").as_str());
        assert_eq!(sent["origin"], base.origin().ascii_serialization().as_str());
        assert_eq!(sent[SESSION_HEADER], headers[0][SESSION_HEADER]);
    }
    let log = log.text();
    assert!(log.contains("refused `leak` (HTTP 500"), "{log}");
    assert!(log.contains("[redacted]") && !log.contains(GRANT), "{log}");

    // A daemon that cannot be reached is an error too, still without it.
    let dead = Url::parse("http://127.0.0.1:9").unwrap();
    let log = Captured::default();
    let mut agent = Agent::start(&dead, log.clone());
    agent
        .send(json!({"jsonrpc":"2.0","id":3,"method":"tools/list"}))
        .await;
    let (line, unreachable) = agent.reply().await;
    assert_eq!(unreachable["error"]["code"], SERVER_ERROR);
    assert!(line.contains("could not be reached") && !line.contains(GRANT));
    agent.close().await;
    assert!(!log.text().contains(GRANT));
}

#[tokio::test]
async fn tool_calls_reach_the_daemon_one_at_a_time_in_order() {
    let (base, seen) = daemon().await;
    let mut agent = Agent::start(&base, Captured::default());
    for id in 1..=3 {
        agent
            .send(json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"work_status"}}))
            .await;
    }
    agent
        .send(json!({"jsonrpc":"2.0","id":"ping","method":"ping"}))
        .await;
    let mut ids = Vec::new();
    for _ in 0..4 {
        ids.push(agent.reply().await.1["id"].clone());
    }
    // The ping is not held behind the calls; the calls keep their order.
    assert_eq!(ids[0], "ping");
    assert_eq!(ids[1..], [json!(1), json!(2), json!(3)]);
    assert_eq!(seen.most_calls_in_flight.load(Ordering::SeqCst), 1);
    agent.close().await;
}

#[tokio::test]
async fn a_bad_line_is_answered_and_the_bridge_reads_on() {
    let (base, _) = daemon().await;
    let mut agent = Agent::start(&base, Captured::default());
    // Not UTF-8, then not JSON: each answered, neither fatal.
    for bad in [&b"\xff\xfe{}\n"[..], b"{not json\n"] {
        agent.send_raw(bad).await;
        let (_, refused) = agent.reply().await;
        assert_eq!(refused["error"]["code"], PARSE_ERROR);
        assert_eq!(refused["id"], Value::Null);
    }
    agent
        .send(json!({"jsonrpc":"2.0","id":1,"method":"ping"}))
        .await;
    assert_eq!(agent.reply().await.1["id"], 1);
    agent.close().await;
}

#[tokio::test]
async fn a_redirect_is_not_followed() {
    let (base, seen) = daemon().await;
    let mut agent = Agent::start(&base, Captured::default());
    agent
        .send(json!({"jsonrpc":"2.0","id":1,"method":"redirect"}))
        .await;
    let (line, refused) = agent.reply().await;
    assert_eq!(refused["error"]["code"], SERVER_ERROR, "{line}");
    assert!(line.contains("HTTP 307"), "{line}");
    assert_eq!(seen.redirected.load(Ordering::SeqCst), 0);
    agent.close().await;
}

#[tokio::test]
async fn a_call_cancelled_while_queued_is_never_sent() {
    let (base, seen) = daemon().await;
    let mut agent = Agent::start(&base, Captured::default());
    for id in 1..=3 {
        agent
            .send(json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"work_status"}}))
            .await;
    }
    // Call 1 is at the daemon and 2 and 3 wait behind it; 3 is cancelled.
    agent
        .send(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":3}}))
        .await;
    let mut ids = Vec::new();
    for _ in 0..2 {
        ids.push(agent.reply().await.1["id"].clone());
    }
    assert_eq!(ids, [json!(1), json!(2)]);
    // The next reply answers the next request, not the cancelled call.
    agent
        .send(json!({"jsonrpc":"2.0","id":4,"method":"ping"}))
        .await;
    assert_eq!(agent.reply().await.1["id"], 4);
    assert_eq!(seen.calls.load(Ordering::SeqCst), 2);
    agent.close().await;
}

#[test]
fn the_grant_comes_from_the_environment_alone() {
    assert_eq!(grant(&[], Some(format!(" {GRANT}\n"))).unwrap(), GRANT);
    for env in [None, Some(String::new()), Some("  ".into())] {
        let refused = grant(&[], env).unwrap_err().to_string();
        assert!(refused.contains(GRANT_ENV), "{refused}");
    }
    // An argument is refused, and never repeated back.
    let refused = grant(&[GRANT.into()], Some(GRANT.into()))
        .unwrap_err()
        .to_string();
    assert!(refused.contains("takes no arguments") && !refused.contains(GRANT));
}

#[test]
fn only_a_loopback_daemon_is_sent_the_grant() {
    for local in [
        "http://127.0.0.1:3311",
        "http://[::1]:3311",
        "http://127.0.0.2:1",
    ] {
        assert!(
            Bridge::new(
                &Url::parse(local).unwrap(),
                GRANT,
                Box::new(Captured::default())
            )
            .is_ok(),
            "{local}"
        );
    }
    for remote in [
        "http://localhost:3311",
        "http://192.168.1.2:3311",
        "https://mac.example.ts.net",
    ] {
        let Err(refused) = Bridge::new(
            &Url::parse(remote).unwrap(),
            GRANT,
            Box::new(Captured::default()),
        ) else {
            panic!("{remote} was accepted");
        };
        let refused = refused.to_string();
        assert!(
            refused.contains("loopback") && !refused.contains(GRANT),
            "{refused}"
        );
    }
    let Err(refused) = Bridge::new(
        &Url::parse("http://127.0.0.1:3311").unwrap(),
        "bad\ngrant",
        Box::new(Captured::default()),
    ) else {
        panic!("a grant that is not a header value was accepted");
    };
    assert!(!refused.to_string().contains("bad\ngrant"));
}
