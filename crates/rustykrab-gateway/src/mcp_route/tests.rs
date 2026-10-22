use super::*;
use async_trait::async_trait;
use axum::body::Body;
use rustykrab_agent::mcp_grants::{CallOutcome, GrantSpec};
use rustykrab_core::model::{ModelProvider, ModelResponse};
use rustykrab_core::types::{Message, ToolSchema};
use std::sync::Arc;
use std::time::Duration;

const OWNER_TOKEN: &str = "owner-token";

struct Unused;
#[async_trait]
impl ModelProvider for Unused {
    fn name(&self) -> &str {
        "unused"
    }
    async fn chat(&self, _: &[Message], _: &[ToolSchema]) -> rustykrab_core::Result<ModelResponse> {
        panic!("the MCP route must not invoke a model");
    }
}

struct Server {
    base: String,
    grants: Arc<McpGrants>,
    backend: Arc<rustykrab_tools::work_backend::StubWorkBackend>,
    task: tokio::task::JoinHandle<()>,
    dir: std::path::PathBuf,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The real router's state over a fresh store in `dir`, its rate limiter at
/// `max_requests` a minute, resolving grants in `grants`.
fn state(dir: &std::path::Path, grants: Arc<McpGrants>, max_requests: u32) -> AppState {
    let store = rustykrab_store::Store::open(dir, vec![7; 32]).unwrap();
    let mut state =
        AppState::new(store, vec![], Arc::new(Unused), OWNER_TOKEN.into()).with_mcp_grants(grants);
    state.rate_limiter = Arc::new(crate::rate_limit::RateLimiter::new(
        crate::RateLimitConfig {
            max_requests,
            window: Duration::from_secs(60),
            lockout: Duration::from_secs(300),
        },
    ));
    state
}

/// The real router on a loopback port, its grants shared with the test.
async fn serve(max_requests: u32) -> Server {
    let dir = std::env::temp_dir().join(format!("rk-mcp-route-{}", uuid::Uuid::new_v4()));
    let grants = Arc::new(McpGrants::new());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = crate::router(state(&dir, grants.clone(), max_requests));
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    Server {
        base,
        grants,
        backend: Arc::new(rustykrab_tools::work_backend::StubWorkBackend::new()),
        task,
        dir,
    }
}

fn status_list(id: u64) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"tools/list"})
}

impl Server {
    fn mint(&self, read_only: bool) -> rustykrab_agent::mcp_grants::McpGrant {
        self.grants.mint(GrantSpec {
            item: "item-a".into(),
            conversation: uuid::Uuid::new_v4(),
            actor: "worker:pinch".into(),
            read_only,
            tools: rustykrab_agent::mcp_grants::work_tools(self.backend.clone()),
            deadline: tokio::time::Instant::now() + Duration::from_secs(600),
            quota: 20,
        })
    }

    /// One message as the bridge sends it.
    async fn post(&self, token: &str, message: Value) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!("{}{PATH}", self.base))
            .bearer_auth(token)
            .header("Origin", &self.base)
            .header(SESSION_HEADER, "session-1")
            .json(&message)
            .send()
            .await
            .unwrap()
    }

    async fn rpc(&self, token: &str, message: Value) -> Value {
        let response = self.post(token, message).await;
        assert_eq!(response.status(), 200);
        response.json().await.unwrap()
    }
}

#[tokio::test]
async fn claude_codes_and_codexs_own_openings_connect_and_call_a_granted_tool() {
    let server = serve(1000).await;
    let grant = server.mint(false);
    let token = grant.token().expose().to_string();
    // Claude Code 2.1.295's opening, as it sent it to a stdio server.
    let discover = server
        .rpc(
            &token,
            json!({"jsonrpc":"2.0","id":"server-discover-probe-1","method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}),
        )
        .await;
    assert_eq!(discover["id"], "server-discover-probe-1");
    assert_eq!(discover["error"]["code"], METHOD_NOT_FOUND);
    let init = server
        .rpc(
            &token,
            json!({"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{"roots":{"listChanged":true}},"clientInfo":{"name":"claude-code","version":"2.1.295"}},"jsonrpc":"2.0","id":0}),
        )
        .await;
    assert_eq!(init["id"], 0);
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(init["result"]["serverInfo"]["name"], "rustykrab");
    // Only what is served is advertised, with no nulls a client rejects.
    assert_eq!(
        init["result"]["capabilities"],
        json!({"tools": {"listChanged": false}})
    );
    let initialized = server
        .post(
            &token,
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        )
        .await;
    assert_eq!(initialized.status(), 202);
    let listed = server
        .rpc(
            &token,
            json!({"method":"tools/list","jsonrpc":"2.0","id":1}),
        )
        .await;
    let mut names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec!["ask_user", "capability_request", "work_file", "work_status"]
    );
    assert!(listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .all(|tool| tool["inputSchema"]["type"] == "object"));
    let called = server
        .rpc(
            &token,
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"work_status","arguments":{"ids":["item-a"]}}}),
        )
        .await;
    assert_eq!(called["result"]["isError"], false, "{called}");
    assert_eq!(called["result"]["content"][0]["type"], "text");
    // Codex 0.159.3 asks for an older revision, which is served as asked;
    // one neither CLI uses gets the newest.
    for (asked, served) in [
        ("2025-06-18", "2025-06-18"),
        ("2024-11-05", "2025-11-25"),
        ("2099-01-01", "2025-11-25"),
    ] {
        let init = server
            .rpc(
                &token,
                json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":asked}}),
            )
            .await;
        assert_eq!(init["result"]["protocolVersion"], served);
    }
    let ping = server
        .rpc(&token, json!({"jsonrpc":"2.0","id":"p","method":"ping"}))
        .await;
    assert_eq!(ping["result"], json!({}));
    let record = grant.finish(None);
    assert!(record.bridge_connected.is_some());
    assert_eq!(record.calls.len(), 1);
    assert_eq!(record.calls[0].tool, "work_status");
    assert_eq!(record.calls[0].outcome, CallOutcome::Ok);
}

#[tokio::test]
async fn what_a_live_grant_refuses_is_a_json_rpc_error() {
    let server = serve(1000).await;
    let grant = server.mint(false);
    let token = grant.token().expose().to_string();
    let outside = server
        .rpc(
            &token,
            json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"exec","arguments":{"command":"id"}}}),
        )
        .await;
    assert_eq!(outside["id"], 7);
    assert_eq!(outside["error"]["code"], INVALID_PARAMS);
    // An id is used once in a session, whatever it is sent with.
    let status = json!({"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"work_status","arguments":{"ids":["item-a"]}}});
    assert_eq!(
        server.rpc(&token, status.clone()).await["result"]["isError"],
        false
    );
    assert_eq!(
        server.rpc(&token, status).await["error"]["code"],
        SERVER_ERROR
    );
    assert_eq!(server.backend.calls().len(), 1);
    // A call the tool refused is an error to the agent, as it is to the record.
    let rejected = server
        .rpc(
            &token,
            json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"ask_user","arguments":{"text":"Which?","item":"item-b"}}}),
        )
        .await;
    assert_eq!(rejected["result"]["isError"], true, "{rejected}");
    // A question that parks the item ends the run; nothing more is called.
    let parked = server
        .rpc(
            &token,
            json!({"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"ask_user","arguments":{"text":"Which?"}}}),
        )
        .await;
    assert!(parked["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("write your final report"));
    let after = server
        .rpc(
            &token,
            json!({"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"work_status","arguments":{"ids":["item-a"]}}}),
        )
        .await;
    assert_eq!(after["error"]["code"], SERVER_ERROR, "{after}");
    assert_eq!(server.backend.calls().len(), 2);
    // A batch is refused whole.
    let batch = server
        .rpc(&token, json!([status_list(1), status_list(2)]))
        .await;
    assert_eq!(batch["error"]["code"], INVALID_REQUEST);
    assert_eq!(batch["id"], Value::Null);

    let review = server.mint(true);
    let review_token = review.token().expose().to_string();
    let listed = server
        .rpc(
            &review_token,
            json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        )
        .await;
    assert_eq!(listed["result"]["tools"], json!([]));
    let refused = server
        .rpc(
            &review_token,
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"work_status","arguments":{}}}),
        )
        .await;
    assert_eq!(refused["error"]["code"], INVALID_PARAMS);
    assert_eq!(server.backend.calls().len(), 2);
}

#[tokio::test]
async fn only_a_live_grant_opens_the_route_and_it_opens_no_other() {
    let server = serve(1000).await;
    let grant = server.mint(false);
    let token = grant.token().expose().to_string();
    let list = json!({"jsonrpc":"2.0","id":1,"method":"tools/list"});
    for presented in [OWNER_TOKEN, "rkmcp_guess", ""] {
        assert_eq!(
            server.post(presented, list.clone()).await.status(),
            401,
            "{presented:?}"
        );
    }
    let client = reqwest::Client::new();
    for path in ["/api/conversations", "/api/work", "/api/version"] {
        let response = client
            .get(format!("{}{path}", server.base))
            .bearer_auth(&token)
            .header("Origin", &server.base)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401, "{path}");
    }
    assert_eq!(server.post(&token, list.clone()).await.status(), 200);
    // The route reads the bearer exactly as the rate limiter does.
    assert_eq!(
        server
            .post(&format!(" {token} "), list.clone())
            .await
            .status(),
        200
    );
    drop(grant.finish(None));
    assert_eq!(server.post(&token, list).await.status(), 401);
}

#[tokio::test]
async fn a_live_grant_is_not_rate_limited_and_a_guess_still_is() {
    let server = serve(2).await;
    let grant = server.mint(false);
    let token = grant.token().expose().to_string();
    for id in 0..6 {
        let listed = server
            .post(
                &token,
                json!({"jsonrpc":"2.0","id":id,"method":"tools/list"}),
            )
            .await;
        assert_eq!(listed.status(), 200, "request {id}");
    }
    let guess = json!({"jsonrpc":"2.0","id":1,"method":"tools/list"});
    let mut statuses = Vec::new();
    for _ in 0..3 {
        statuses.push(server.post("rkmcp_guess", guess.clone()).await.status());
    }
    assert_eq!(statuses, vec![401, 401, 429]);
}

#[tokio::test]
async fn the_real_router_refuses_a_live_grant_from_another_machine() {
    use tower::ServiceExt;
    let server = serve(1000).await;
    let grant = server.mint(false);
    let app = crate::router(state(
        &server.dir.join("remote"),
        server.grants.clone(),
        1000,
    ));
    let send = |peer: &str, host: &str| {
        let mut request = Request::builder()
            .method("POST")
            .uri(PATH)
            .header(
                "authorization",
                format!("Bearer {}", grant.token().expose()),
            )
            .header("host", host)
            .header("origin", "http://127.0.0.1:3000")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}).to_string(),
            ))
            .unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
        app.clone().oneshot(request)
    };
    assert_eq!(
        send("192.168.1.2:4567", "127.0.0.1:3000")
            .await
            .unwrap()
            .status(),
        403
    );
    // `tailscale serve` connects from loopback and keeps the outside host.
    assert_eq!(
        send("127.0.0.1:4567", "mac.example.ts.net")
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        send("127.0.0.1:4567", "127.0.0.1:3000")
            .await
            .unwrap()
            .status(),
        200
    );
}

#[test]
fn only_a_local_connection_for_a_local_host_nothing_forwarded_is_a_caller() {
    fn request(peer: Option<&str>, host: Option<&str>, extra: Option<(&str, &str)>) -> Request {
        let mut request = Request::builder()
            .method("POST")
            .uri(PATH)
            .body(Body::empty())
            .unwrap();
        if let Some(peer) = peer {
            request
                .extensions_mut()
                .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
        }
        if let Some(host) = host {
            request
                .headers_mut()
                .insert(header::HOST, host.parse().unwrap());
        }
        if let Some((name, value)) = extra {
            request.headers_mut().insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        request
    }
    const HOST: Option<&str> = Some("127.0.0.1:3311");
    for peer in ["127.0.0.1:4567", "[::1]:4567", "[::ffff:127.0.0.1]:4567"] {
        assert!(local_caller(&request(Some(peer), HOST, None)), "{peer}");
    }
    for host in ["localhost:3311", "[::1]:3311"] {
        assert!(local_caller(&request(
            Some("127.0.0.1:4567"),
            Some(host),
            None
        )));
    }
    // The connection's own peer decides; a header claiming loopback does not.
    for peer in [None, Some("192.168.1.2:4567"), Some("100.101.102.103:4567")] {
        assert!(!local_caller(&request(
            peer,
            HOST,
            Some(("x-forwarded-for", "127.0.0.1"))
        )));
        assert!(!local_caller(&request(peer, HOST, None)));
    }
    // A local proxy connects from loopback, but keeps the outside host...
    for host in [
        None,
        Some("mac.example.ts.net"),
        Some("mac.example.ts.net:443"),
    ] {
        assert!(!local_caller(&request(Some("127.0.0.1:4567"), host, None)));
    }
    // ...and says what it forwarded, and for whom.
    for extra in [
        ("x-forwarded-for", "100.101.102.103"),
        ("forwarded", "for=100.101.102.103"),
        ("x-forwarded-host", "mac.example.ts.net"),
        ("x-forwarded-proto", "https"),
        ("x-real-ip", "100.101.102.103"),
        ("via", "1.1 proxy"),
        ("cf-connecting-ip", "100.101.102.103"),
        ("tailscale-user-login", "owner@example.com"),
    ] {
        assert!(!local_caller(&request(
            Some("127.0.0.1:4567"),
            HOST,
            Some(extra)
        )));
    }
}
