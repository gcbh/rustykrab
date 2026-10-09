use super::*;
use async_trait::async_trait;
use axum::body::Body;
use rustykrab_core::{
    model::{ModelProvider, ModelResponse},
    types::{Message, ToolSchema},
};
use std::{sync::Arc, time::Duration};

const ORIGIN: &str = "https://fixture.example.ts.net:8443";
const HOST: &str = "fixture.example.ts.net:8443";
const OWNER: &str = "owner@example.com";

fn policy() -> TailnetAuthPolicy {
    TailnetAuthPolicy::new(ORIGIN, vec![OWNER.into()]).unwrap()
}
fn request(peer: Option<&str>, host: &str, login: &str) -> Request {
    let mut req = Request::builder()
        .uri("/api/conversations")
        .header("Host", host)
        .header("Tailscale-User-Login", login)
        .body(Body::empty())
        .unwrap();
    if let Some(peer) = peer {
        req.extensions_mut()
            .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
    }
    req
}

#[test]
fn pins_origin_explicit_owners_and_kernel_peer() {
    let policy = policy();
    let owner = policy
        .principal(&request(Some("127.0.0.1:4567"), HOST, "OWNER@example.com"))
        .unwrap();
    assert_eq!(owner.describe(), "tailscale:owner@example.com");
    assert!(policy
        .principal(&request(Some("[::1]:4567"), HOST, OWNER))
        .is_some());
    for peer in [None, Some("100.101.102.103:4567"), Some("192.168.1.2:4567")] {
        let mut req = request(peer, HOST, OWNER);
        req.headers_mut()
            .insert("x-forwarded-for", "127.0.0.1".parse().unwrap());
        assert!(policy.principal(&req).is_none());
    }
    for (host, login) in [
        (HOST, "guest@example.com"),
        (HOST, ""),
        ("localhost:3311", OWNER),
        ("other.ts.net:8443", OWNER),
        ("fixture.example.ts.net", OWNER),
    ] {
        assert!(policy
            .principal(&request(Some("127.0.0.1:4567"), host, login))
            .is_none());
    }
    for name in ["host", "tailscale-user-login"] {
        let mut req = request(Some("127.0.0.1:4567"), HOST, OWNER);
        req.headers_mut().append(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            OWNER.parse().unwrap(),
        );
        assert!(policy.principal(&req).is_none());
    }
    for origin in [
        "http://fixture.example.ts.net",
        "https://example.com",
        "https://fixture.example.ts.net/path",
        "https://fixture.example.ts.net/?q=1",
        "https://owner@fixture.example.ts.net",
        "not a URL",
    ] {
        assert!(TailnetAuthPolicy::new(origin, vec![OWNER.into()]).is_err());
    }
    for logins in [
        vec![],
        vec![" ".into()],
        vec!["*".into()],
        vec!["owner example".into()],
    ] {
        assert!(TailnetAuthPolicy::new(ORIGIN, logins).is_err());
    }
}

struct Unused;
#[async_trait]
impl ModelProvider for Unused {
    fn name(&self) -> &str {
        "unused"
    }
    async fn chat(&self, _: &[Message], _: &[ToolSchema]) -> rustykrab_core::Result<ModelResponse> {
        panic!("Viewing and authenticating must not invoke a model");
    }
}

struct Server {
    base: String,
    store: rustykrab_store::Store,
    task: tokio::task::JoinHandle<()>,
    dir: std::path::PathBuf,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
async fn serve(enabled: bool, max_requests: u32) -> Server {
    let dir = std::env::temp_dir().join(format!("rk-tailnet-auth-{}", uuid::Uuid::new_v4()));
    let store = rustykrab_store::Store::open(&dir, vec![7; 32]).unwrap();
    let mut state = AppState::new(store.clone(), vec![], Arc::new(Unused), "test-token".into())
        .with_origin_policy(crate::OriginPolicy::new([ORIGIN.to_string()]));
    if enabled {
        state.tailnet_auth = Some(policy());
    }
    state.rate_limiter = Arc::new(crate::rate_limit::RateLimiter::new(
        crate::RateLimitConfig {
            max_requests,
            window: Duration::from_secs(60),
            lockout: Duration::from_secs(300),
        },
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = crate::router(state);
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
        store,
        task,
        dir,
    }
}
fn owner_request(client: &reqwest::Client, server: &Server, path: &str) -> reqwest::RequestBuilder {
    client
        .get(format!("{}{}", server.base, path))
        .header("Host", HOST)
        .header("Origin", ORIGIN)
        .header("Tailscale-User-Login", OWNER)
}

#[tokio::test]
async fn real_router_owner_identity_and_commands_preserve_csrf_and_token_auth() {
    let server = serve(true, 1000).await;
    let client = reqwest::Client::new();
    let status = owner_request(&client, &server, "/api/access")
        .send()
        .await
        .unwrap();
    assert_eq!(status.headers().get("cache-control").unwrap(), "no-store");
    let body: serde_json::Value = status.json().await.unwrap();
    assert_eq!(body["method"], "tailscale");
    assert_eq!(body["identity"], OWNER);
    assert_eq!(body["authenticated"], true);
    assert!(body.get("token").is_none());

    let created = client
        .post(format!("{}/api/conversations", server.base))
        .header("Host", HOST)
        .header("Origin", ORIGIN)
        .header("Tailscale-User-Login", OWNER)
        .json(&serde_json::json!({"title":"Owner browser"}))
        .send()
        .await
        .unwrap();
    assert!(created.status().is_success());
    let created: serde_json::Value = created.json().await.unwrap();
    assert_eq!(
        server.store.conversations().list_ids().await.unwrap().len(),
        1
    );
    assert!(created["id"].is_string());

    let browser = client
        .get(format!("{}/api/conversations", server.base))
        .header("Host", HOST)
        .header("Tailscale-User-Login", OWNER)
        .header("Sec-Fetch-Site", "same-origin")
        .header("Sec-Fetch-Mode", "cors")
        .header("Sec-Fetch-Dest", "empty")
        .header("Referer", format!("{ORIGIN}/"));
    assert_eq!(browser.send().await.unwrap().status(), 200);
    for origin in [None, Some("https://outside.example")] {
        let mut req = client
            .post(format!("{}/api/conversations", server.base))
            .header("Host", HOST)
            .header("Tailscale-User-Login", OWNER)
            .json(&serde_json::json!({}));
        if let Some(origin) = origin {
            req = req.header("Origin", origin);
        }
        assert_eq!(req.send().await.unwrap().status(), 403);
    }
    for (host, login) in [
        (HOST, "guest@example.com"),
        ("127.0.0.1:3311", OWNER),
        (HOST, ""),
    ] {
        let req = client
            .get(format!("{}/api/conversations", server.base))
            .header("Origin", ORIGIN)
            .header("Host", host)
            .header("Tailscale-User-Login", login);
        assert_eq!(req.send().await.unwrap().status(), 401);
    }
    for token in ["wrong", ""] {
        assert_eq!(
            owner_request(&client, &server, "/api/conversations")
                .bearer_auth(token)
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
    }
    assert_eq!(
        client
            .get(format!("{}/api/conversations", server.base))
            .header("Origin", &server.base)
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let code = server.store.devices().mint_pairing_code().await.unwrap();
    let (_, token) = server
        .store
        .devices()
        .redeem_pairing_code(&code, "Paired browser")
        .await
        .unwrap();
    let paired: serde_json::Value = client
        .get(format!("{}/api/access", server.base))
        .header("Origin", &server.base)
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(paired["method"], "token");
    assert_eq!(paired["identity"], "Paired browser");
    let anonymous: serde_json::Value = client
        .get(format!("{}/api/access", server.base))
        .header("Origin", &server.base)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(anonymous["authenticated"], false);
    assert!(anonymous["identity"].is_null());
    assert_eq!(
        server.store.conversations().list_ids().await.unwrap().len(),
        1
    );
}

#[tokio::test]
async fn opt_in_required_and_allowed_owner_polling_cannot_be_locked_out() {
    let client = reqwest::Client::new();
    let disabled = serve(false, 100).await;
    assert_eq!(
        owner_request(&client, &disabled, "/api/conversations")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let status: serde_json::Value = owner_request(&client, &disabled, "/api/access")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["tailscale_enabled"], false);
    assert_eq!(status["authenticated"], false);

    let enabled = serve(true, 1).await;
    for _ in 0..3 {
        let _ = client
            .get(format!("{}/api/conversations", enabled.base))
            .header("Origin", &enabled.base)
            .send()
            .await
            .unwrap();
    }
    for _ in 0..25 {
        assert_eq!(
            owner_request(&client, &enabled, "/api/conversations")
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    }
    assert_eq!(
        owner_request(&client, &enabled, "/api/conversations")
            .bearer_auth("wrong")
            .send()
            .await
            .unwrap()
            .status(),
        429
    );
}
