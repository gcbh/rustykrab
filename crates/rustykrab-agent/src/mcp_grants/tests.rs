use super::*;
use async_trait::async_trait;
use rustykrab_tools::work_backend::{with_work_run, StubWorkBackend, WorkCall};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Semaphore;

/// Answers with the run binding it executed under and the arguments it was
/// given, counting its executions.
struct Probe {
    name: &'static str,
    runs: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for Probe {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "fixture"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name.into(),
            description: "fixture".into(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }
    async fn execute(&self, args: Value) -> Result<Value> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        let bound = with_work_run(|run| (run.item.clone(), run.actor.clone()));
        Ok(serde_json::json!({"bound": bound, "args": args}))
    }
}

/// Runs until the test lets it finish: `started` gets a permit when it
/// starts, and it finishes when `release` gives it one.
struct Gate {
    started: Arc<Semaphore>,
    release: Arc<Semaphore>,
}

#[async_trait]
impl Tool for Gate {
    fn name(&self) -> &str {
        "gate"
    }
    fn description(&self) -> &str {
        "fixture"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "gate".into(),
            description: "fixture".into(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }
    async fn execute(&self, _args: Value) -> Result<Value> {
        self.started.add_permits(1);
        let _permit = self.release.acquire().await.expect("open");
        Ok(serde_json::json!({"finished": true}))
    }
}

/// Never finishes.
struct Hang;

#[async_trait]
impl Tool for Hang {
    fn name(&self) -> &str {
        "hang"
    }
    fn description(&self) -> &str {
        "fixture"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "hang".into(),
            description: "fixture".into(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }
    async fn execute(&self, _args: Value) -> Result<Value> {
        std::future::pending().await
    }
}

fn probe() -> (Arc<dyn Tool>, Arc<AtomicUsize>) {
    let runs = Arc::new(AtomicUsize::new(0));
    (
        Arc::new(Probe {
            name: "probe",
            runs: runs.clone(),
        }),
        runs,
    )
}

fn gate() -> (Arc<dyn Tool>, Arc<Semaphore>, Arc<Semaphore>) {
    let started = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    (
        Arc::new(Gate {
            started: started.clone(),
            release: release.clone(),
        }),
        started,
        release,
    )
}

fn spec(item: &str, run: &str, tools: Vec<Arc<dyn Tool>>) -> GrantSpec {
    GrantSpec {
        item: item.into(),
        run: run.into(),
        actor: "worker:pinch".into(),
        read_only: false,
        tools,
        deadline: Instant::now() + Duration::from_secs(3600),
        quota: 10,
    }
}

fn outcomes(record: &GrantRecord) -> Vec<(&str, CallOutcome)> {
    record
        .calls
        .iter()
        .map(|call| (call.tool.as_str(), call.outcome))
        .collect()
}

#[tokio::test]
async fn a_call_runs_in_the_grants_scope_whatever_its_arguments_name() {
    let grants = Arc::new(McpGrants::new());
    let (tool, runs) = probe();
    let grant = grants.mint(spec("item-a", "run-1", vec![tool]));
    let token = grant.token().expose().to_string();
    // The agent names another item, run and actor; the grant's are used.
    let forged = serde_json::json!({"item": "item-b", "run": "run-2", "actor": "user:master"});
    let answer = grants
        .call(&token, "s:1", "probe", forged.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        answer["bound"],
        serde_json::json!(["item-a", "worker:pinch"])
    );
    assert_eq!(answer["args"], forged);
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    let record = grant.finish();
    assert_eq!(outcomes(&record), vec![("probe", CallOutcome::Ok)]);
    assert_eq!(
        (record.item.as_str(), record.run.as_str()),
        ("item-a", "run-1")
    );
}

#[tokio::test]
async fn the_work_tools_bind_the_grants_item_and_actor_through_the_real_backend() {
    let backend = Arc::new(StubWorkBackend::new());
    let grants = Arc::new(McpGrants::new());
    let tools = work_tools(backend.clone());
    let mut names: Vec<&str> = tools.iter().map(|tool| tool.name()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec!["ask_user", "capability_request", "work_file", "work_status"]
    );
    let grant = grants.mint(spec("item-a", "run-1", tools));
    let token = grant.token().expose().to_string();
    grants
        .call(
            &token,
            "s:1",
            "work_status",
            serde_json::json!({"ids": ["item-b"]}),
        )
        .await
        .unwrap()
        .unwrap();
    // A question names no item: the grant's is the one that is parked.
    grants
        .call(
            &token,
            "s:2",
            "ask_user",
            serde_json::json!({"text": "Which calendar?"}),
        )
        .await
        .unwrap()
        .unwrap();
    // An item named in the arguments is refused by the tool's own checks.
    let forged = grants
        .call(
            &token,
            "s:3",
            "ask_user",
            serde_json::json!({"text": "Which calendar?", "item": "item-b"}),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(forged["outcome"], "rejected", "{forged}");
    let calls = backend.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(matches!(&calls[0], WorkCall::Status { principal, .. }
        if principal.item.as_deref() == Some("item-a")));
    assert!(matches!(&calls[1], WorkCall::Ask { item, provenance, .. }
        if item == "item-a"
            && provenance.actor == "worker:pinch"
            && provenance.filed_by_item.as_deref() == Some("item-a")));
    assert_eq!(
        outcomes(&grant.finish()),
        vec![
            ("work_status", CallOutcome::Ok),
            ("ask_user", CallOutcome::Ok),
            ("ask_user", CallOutcome::Ok),
        ]
    );
}

#[tokio::test]
async fn a_call_outside_the_grant_is_refused_and_recorded_as_denied() {
    let grants = Arc::new(McpGrants::new());
    let (tool, runs) = probe();
    let grant = grants.mint(spec("item-a", "run-1", vec![tool]));
    let token = grant.token().expose().to_string();
    assert_eq!(
        grants
            .call(&token, "s:1", "exec", serde_json::json!({"command": "id"}))
            .await,
        Err(CallError::NotGranted)
    );
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    let listed: Vec<String> = grants
        .list(&token)
        .unwrap()
        .into_iter()
        .map(|schema| schema.name)
        .collect();
    assert_eq!(listed, vec!["probe"]);
    assert_eq!(
        outcomes(&grant.finish()),
        vec![("exec", CallOutcome::Denied)]
    );
}

#[tokio::test]
async fn a_read_only_review_is_granted_no_tools() {
    let grants = Arc::new(McpGrants::new());
    let (tool, runs) = probe();
    let grant = grants.mint(GrantSpec {
        read_only: true,
        ..spec("review", "run-1", vec![tool])
    });
    let token = grant.token().expose().to_string();
    assert!(grants.list(&token).unwrap().is_empty());
    assert_eq!(
        grants
            .call(&token, "s:1", "probe", serde_json::json!({}))
            .await,
        Err(CallError::ReadOnly)
    );
    assert_eq!(runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn every_attempt_counts_against_the_quota() {
    let grants = Arc::new(McpGrants::new());
    let (tool, runs) = probe();
    let grant = grants.mint(GrantSpec {
        quota: 2,
        ..spec("item-a", "run-1", vec![tool])
    });
    let token = grant.token().expose().to_string();
    assert!(grants
        .call(&token, "s:1", "probe", serde_json::json!({}))
        .await
        .is_ok());
    // A refused attempt counts too, and a new session is the same run.
    assert_eq!(
        grants
            .call(&token, "s:2", "exec", serde_json::json!({}))
            .await,
        Err(CallError::NotGranted)
    );
    assert_eq!(
        grants
            .call(&token, "t:1", "probe", serde_json::json!({}))
            .await,
        Err(CallError::QuotaSpent)
    );
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn one_call_is_in_flight_per_grant() {
    let grants = Arc::new(McpGrants::new());
    let (tool, started, release) = gate();
    let (other, _) = probe();
    let grant = grants.mint(spec("item-a", "run-1", vec![tool, other]));
    let token = grant.token().expose().to_string();
    let first = tokio::spawn({
        let (grants, token) = (grants.clone(), token.clone());
        async move {
            grants
                .call(&token, "s:1", "gate", serde_json::json!({}))
                .await
        }
    });
    started.acquire().await.unwrap().forget();
    for (retry, tool) in [("s:2", "probe"), ("s:3", "gate"), ("s:1", "gate")] {
        assert_eq!(
            grants
                .call(&token, retry, tool, serde_json::json!({}))
                .await,
            Err(CallError::Busy),
            "{retry} {tool}"
        );
    }
    release.add_permits(1);
    assert_eq!(
        first.await.unwrap(),
        Ok(Ok(serde_json::json!({"finished": true})))
    );
    assert!(grants
        .call(&token, "s:4", "probe", serde_json::json!({}))
        .await
        .is_ok());
}

#[tokio::test]
async fn revocation_stops_admission_and_cancels_the_call_in_flight() {
    let grants = Arc::new(McpGrants::new());
    let (tool, started, _release) = gate();
    let grant = grants.mint(spec("item-a", "run-1", vec![tool]));
    let token = grant.token().expose().to_string();
    let in_flight = tokio::spawn({
        let (grants, token) = (grants.clone(), token.clone());
        async move {
            grants
                .call(&token, "s:1", "gate", serde_json::json!({}))
                .await
        }
    });
    started.acquire().await.unwrap().forget();
    let record = grant.finish();
    assert_eq!(outcomes(&record), vec![("gate", CallOutcome::Ambiguous)]);
    assert_eq!(in_flight.await.unwrap(), Err(CallError::Cancelled));
    assert!(!grants.is_live(&token));
    assert_eq!(grants.initialize(&token), Err(CallError::NotLive));
    assert_eq!(
        grants
            .call(&token, "s:2", "gate", serde_json::json!({}))
            .await,
        Err(CallError::NotLive)
    );
}

#[tokio::test]
async fn a_dropped_grant_is_revoked() {
    let grants = Arc::new(McpGrants::new());
    let grant = grants.mint(spec("item-a", "run-1", vec![]));
    let token = grant.token().expose().to_string();
    assert!(grants.is_live(&token));
    drop(grant);
    assert!(!grants.is_live(&token));
}

#[tokio::test(start_paused = true)]
async fn expiry_stops_admission_and_cancels_the_call_in_flight() {
    let grants = Arc::new(McpGrants::new());
    let grant = grants.mint(GrantSpec {
        deadline: Instant::now() + Duration::from_secs(5),
        ..spec("item-a", "run-1", vec![Arc::new(Hang)])
    });
    let token = grant.token().expose().to_string();
    // The tool's own cap is 30 s; the grant's deadline comes first.
    assert_eq!(
        grants
            .call(&token, "s:1", "hang", serde_json::json!({}))
            .await,
        Err(CallError::Cancelled)
    );
    assert!(!grants.is_live(&token));
    assert_eq!(
        grants
            .call(&token, "s:2", "hang", serde_json::json!({}))
            .await,
        Err(CallError::NotLive)
    );
    assert_eq!(
        outcomes(&grant.finish()),
        vec![("hang", CallOutcome::Ambiguous)]
    );
}

#[tokio::test(start_paused = true)]
async fn a_call_is_clamped_to_the_tools_own_cap() {
    let grants = Arc::new(McpGrants::new());
    let grant = grants.mint(spec("item-a", "run-1", vec![Arc::new(Hang)]));
    let token = grant.token().expose().to_string();
    let started = Instant::now();
    let answer = grants
        .call(&token, "s:1", "hang", serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(
        started.elapsed(),
        Duration::from_secs(tool_cap_secs("hang", &Default::default()))
    );
    assert!(answer
        .unwrap_err()
        .contains("whether it took effect is unknown"));
    // The grant outlives the call, and its retry is told the same.
    assert!(grants.is_live(&token));
    assert_eq!(
        grants
            .call(&token, "s:1", "hang", serde_json::json!({}))
            .await,
        Ok(Err(UNKNOWN_EFFECT.into()))
    );
    assert_eq!(
        outcomes(&grant.finish()),
        vec![("hang", CallOutcome::Ambiguous)]
    );
}

#[tokio::test]
async fn a_retry_repeats_its_answer_and_a_conflicting_payload_is_refused() {
    let grants = Arc::new(McpGrants::new());
    let (tool, runs) = probe();
    let grant = grants.mint(GrantSpec {
        quota: 2,
        ..spec("item-a", "run-1", vec![tool])
    });
    let token = grant.token().expose().to_string();
    let args = serde_json::json!({"n": 1});
    let first = grants
        .call(&token, "s:1", "probe", args.clone())
        .await
        .unwrap();
    // Repeated: answered again, not run again, not counted again.
    for _ in 0..3 {
        assert_eq!(
            grants.call(&token, "s:1", "probe", args.clone()).await,
            Ok(first.clone())
        );
    }
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    assert_eq!(
        grants
            .call(&token, "s:1", "probe", serde_json::json!({"n": 2}))
            .await,
        Err(CallError::ConflictingRetry)
    );
    // The same JSON-RPC id from another bridge session is another call.
    assert!(grants.call(&token, "t:1", "probe", args).await.is_ok());
    assert_eq!(runs.load(Ordering::SeqCst), 2);
    assert_eq!(grant.finish().calls.len(), 2);
}

#[tokio::test]
async fn two_concurrent_runs_on_one_worker_keep_their_own_records() {
    let grants = Arc::new(McpGrants::new());
    let (tool, _) = probe();
    let a = grants.mint(spec("item-a", "run-1", vec![tool.clone()]));
    let b = grants.mint(spec("item-b", "run-2", vec![tool]));
    let (token_a, token_b) = (
        a.token().expose().to_string(),
        b.token().expose().to_string(),
    );
    assert_ne!(token_a, token_b);
    let (answer_a, answer_b, denied_b) = tokio::join!(
        grants.call(&token_a, "s:1", "probe", serde_json::json!({})),
        grants.call(&token_b, "s:1", "probe", serde_json::json!({})),
        grants.call(&token_b, "s:2", "exec", serde_json::json!({})),
    );
    assert_eq!(
        answer_a.unwrap().unwrap()["bound"],
        serde_json::json!(["item-a", "worker:pinch"])
    );
    assert_eq!(
        answer_b.unwrap().unwrap()["bound"],
        serde_json::json!(["item-b", "worker:pinch"])
    );
    assert_eq!(denied_b, Err(CallError::NotGranted));
    let (record_a, record_b) = (a.finish(), b.finish());
    assert_ne!(record_a.generation, record_b.generation);
    assert_eq!(outcomes(&record_a), vec![("probe", CallOutcome::Ok)]);
    assert_eq!(
        outcomes(&record_b),
        vec![("probe", CallOutcome::Ok), ("exec", CallOutcome::Denied)]
    );
    // Neither record attests the other run, nor its own item under another run.
    let mut report = ResultReport::default();
    assert!(record_a.attest("item-b", "run-2", &mut report).is_err());
    assert!(record_a.attest("item-a", "run-2", &mut report).is_err());
    assert!(record_b.attest("item-a", "run-1", &mut report).is_err());
    assert!(report.artifacts.is_empty());
}

#[tokio::test]
async fn a_late_result_from_an_old_run_reaches_no_record() {
    let grants = Arc::new(McpGrants::new());
    let (tool, started, release) = gate();
    let old = grants.mint(spec("item-a", "run-1", vec![tool.clone()]));
    let old_token = old.token().expose().to_string();
    let late = tokio::spawn({
        let (grants, token) = (grants.clone(), old_token.clone());
        async move {
            grants
                .call(&token, "s:1", "gate", serde_json::json!({}))
                .await
        }
    });
    started.acquire().await.unwrap().forget();
    // The old run ends; the item runs again under a new grant.
    let old_live = old.live.clone();
    let old_record = old.finish();
    let new = grants.mint(spec("item-a", "run-2", vec![tool]));
    release.add_permits(1);
    assert_eq!(late.await.unwrap(), Err(CallError::Cancelled));
    assert_eq!(
        outcomes(&old_record),
        vec![("gate", CallOutcome::Ambiguous)]
    );
    // A result that still arrives for the old call is dropped: the old
    // record stays as it was sealed.
    assert!(!old_live.settle("s:1", "gate", Some(Ok(serde_json::json!({})))));
    assert_eq!(old_live.seal(), old_record);
    let new_record = new.finish();
    assert!(new_record.calls.is_empty(), "{new_record:?}");
    assert_ne!(old_record.generation, new_record.generation);
    let mut report = ResultReport::default();
    assert!(old_record.attest("item-a", "run-2", &mut report).is_err());
}

#[tokio::test]
async fn attesting_replaces_the_agents_own_call_claims() {
    let grants = Arc::new(McpGrants::new());
    let (tool, _) = probe();
    let grant = grants.mint(spec("item-a", "run-1", vec![tool]));
    let token = grant.token().expose().to_string();
    grants
        .call(&token, "s:1", "probe", serde_json::json!({}))
        .await
        .unwrap()
        .unwrap();
    let record = grant.finish();
    let mut report = ResultReport {
        artifacts: vec![
            ArtifactRef {
                kind: MCP_CALL.into(),
                value: r#"{"tool":"gmail","outcome":"ok"}"#.into(),
            },
            ArtifactRef {
                kind: "path".into(),
                value: "notes.md".into(),
            },
        ],
        ..ResultReport::default()
    };
    record.attest("item-a", "run-1", &mut report).unwrap();
    assert_eq!(
        report.artifacts,
        vec![
            ArtifactRef {
                kind: "path".into(),
                value: "notes.md".into(),
            },
            ArtifactRef {
                kind: MCP_CALL.into(),
                value: r#"{"tool":"probe","outcome":"ok"}"#.into(),
            },
        ]
    );
}

#[tokio::test]
async fn the_bridge_connects_when_initialize_and_tools_list_both_arrive() {
    let grants = Arc::new(McpGrants::new());
    let initialized_only = grants.mint(spec("item-a", "run-1", vec![]));
    grants
        .initialize(initialized_only.token().expose())
        .unwrap();
    assert_eq!(initialized_only.finish().bridge_connected, None);
    let connected = grants.mint(spec("item-a", "run-2", vec![]));
    let token = connected.token().expose().to_string();
    grants.initialize(&token).unwrap();
    grants.list(&token).unwrap();
    let record = connected.finish();
    assert!(record.bridge_connected.is_some());
    // Reachability is never a call.
    assert!(record.calls.is_empty());
}

#[test]
fn tokens_are_random_prefixed_and_never_printed() {
    let grants = Arc::new(McpGrants::new());
    let a = grants.mint(spec("item-a", "run-1", vec![]));
    let b = grants.mint(spec("item-a", "run-1", vec![]));
    assert!(a.token().expose().starts_with(TOKEN_PREFIX));
    assert_eq!(a.token().expose().len(), TOKEN_PREFIX.len() + 64);
    assert_ne!(a.token(), b.token());
    assert!(!format!("{:?}", a.token()).contains(a.token().expose()));
}
