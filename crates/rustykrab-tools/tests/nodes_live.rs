//! Live delegation test for the `nodes` tool.
//!
//! Ignored by default — needs a peer RustyKrab gateway running and
//! `RUSTYKRAB_NODES` pointing at it:
//!
//! ```sh
//! RUSTYKRAB_NODES='[{"id":"peer","url":"http://127.0.0.1:3100","token":"...","description":"test peer"}]' \
//!   cargo test -p rustykrab-tools --test nodes_live -- --ignored --nocapture
//! ```

use rustykrab_core::Tool;
use rustykrab_tools::NodesTool;
use serde_json::json;

#[tokio::test]
#[ignore = "requires a peer RustyKrab gateway"]
async fn live_delegation_round_trip() {
    let tool = NodesTool::new();
    assert!(tool.available(), "RUSTYKRAB_NODES must be set");

    let listed = tool.execute(json!({"action": "list"})).await.unwrap();
    eprintln!("list: {listed}");
    let id = listed["nodes"][0]["id"]
        .as_str()
        .expect("a node")
        .to_string();

    let discovered = tool.execute(json!({"action": "discover"})).await.unwrap();
    eprintln!("discover: {discovered}");
    assert_eq!(
        discovered["nodes"][0]["status"], "online",
        "peer should be reachable"
    );

    let result = tool
        .execute(json!({
            "action": "send",
            "node_id": id,
            "message": "Reply with exactly: DELEGATED-OK. Nothing else."
        }))
        .await
        .expect("delegation failed");
    eprintln!("send: {result}");

    let response = result["response"].as_str().unwrap_or_default();
    assert!(
        response.contains("DELEGATED-OK"),
        "peer did not answer the delegated task: {response:?}"
    );
    assert!(result["conversation_id"].is_string());
}

#[tokio::test]
#[ignore = "requires a peer RustyKrab gateway"]
async fn unknown_node_is_rejected() {
    let err = NodesTool::new()
        .execute(json!({"action": "send", "node_id": "nope", "message": "hi"}))
        .await
        .expect_err("expected an error");
    assert!(err.to_string().contains("unknown node"), "got: {err}");
}
