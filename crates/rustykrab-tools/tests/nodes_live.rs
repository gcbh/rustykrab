//! Live delegation test for the `nodes` tool.
//!
//! Ignored by default — needs a peer RustyKrab gateway running and
//! `RUSTYKRAB_NODES` pointing at it:
//!
//! ```sh
//! RUSTYKRAB_NODES='[{"id":"peer","url":"http://127.0.0.1:3100","token":"...","description":"test peer"}]' \
//!   cargo test -p rustykrab-tools --test nodes_live -- --ignored --nocapture
//! ```

use std::time::{Duration, Instant};

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

    let handle = tool
        .execute(json!({
            "action": "send",
            "node_id": id,
            "message": "Reply with exactly: DELEGATED-OK. Nothing else."
        }))
        .await
        .expect("delegation failed");
    eprintln!("send: {handle}");

    // Submission returns a handle, not an answer. This is the whole point
    // of the asynchronous path: it comes back in about a second, where the
    // task behind it runs for minutes.
    let task_id = handle["task_id"].as_str().expect("a task id").to_string();

    // Poll until terminal. Generous: a cold turn on a local model spends
    // a minute or more just evaluating its own system prompt.
    let deadline = Instant::now() + Duration::from_secs(600);
    let done = loop {
        let state = tool
            .execute(json!({"action": "check", "node_id": id, "task_id": task_id}))
            .await
            .expect("check failed");
        let status = state["status"].as_str().unwrap_or_default().to_string();
        eprintln!("check: {status} ({}s)", state["elapsed_secs"]);
        if status != "queued" && status != "running" {
            break state;
        }
        assert!(
            Instant::now() < deadline,
            "task never reached a terminal state: {state}"
        );
        tokio::time::sleep(Duration::from_secs(5)).await;
    };

    assert_eq!(done["status"], "done", "task did not succeed: {done}");
    let response = done["response"].as_str().unwrap_or_default();
    assert!(
        response.contains("DELEGATED-OK"),
        "peer did not answer the delegated task: {response:?}"
    );
    // The thread id is what makes a follow-up cheap, so it must survive
    // the round trip.
    assert!(done["conversation_id"].is_string());
}

/// A submitted task can be called off before the node finishes it.
#[tokio::test]
#[ignore = "requires a peer RustyKrab gateway"]
async fn a_submitted_task_can_be_cancelled() {
    let tool = NodesTool::new();
    assert!(tool.available(), "RUSTYKRAB_NODES must be set");

    let listed = tool.execute(json!({"action": "list"})).await.unwrap();
    let id = listed["nodes"][0]["id"]
        .as_str()
        .expect("a node")
        .to_string();

    let handle = tool
        .execute(json!({
            "action": "send",
            "node_id": id,
            "message": "Count slowly to one thousand, showing every number."
        }))
        .await
        .expect("delegation failed");
    let task_id = handle["task_id"].as_str().expect("a task id").to_string();

    let cancelled = tool
        .execute(json!({"action": "cancel", "node_id": id, "task_id": task_id}))
        .await
        .expect("cancel failed");
    assert_eq!(cancelled["status"], "cancelled");

    let after = tool
        .execute(json!({"action": "check", "node_id": id, "task_id": task_id}))
        .await
        .expect("check failed");
    assert_eq!(
        after["status"], "cancelled",
        "a cancelled task must stay cancelled: {after}"
    );
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
