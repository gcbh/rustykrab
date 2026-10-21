use super::*;
use async_trait::async_trait;
use rustykrab_core::active_tools::with_session_context;
use rustykrab_core::Result;
use rustykrab_tools::work_backend::{with_work_run, StubWorkBackend, WorkCall};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Semaphore;

/// Answers with the run binding and conversation it executed under and the
/// arguments it was given, counting its executions.
struct Probe {
    runs: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for Probe {
    fn name(&self) -> &str {
        "probe"
    }
    fn description(&self) -> &str {
        "fixture"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "probe".into(),
            description: "fixture".into(),
            parameters: json!({"type": "object"}),
        }
    }
    async fn execute(&self, args: Value) -> Result<Value> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        let bound = with_work_run(|run| (run.item.clone(), run.actor.clone()));
        let conversation = with_session_context(|session| session.conversation_id);
        Ok(json!({"bound": bound, "conversation": conversation, "args": args}))
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
            parameters: json!({"type": "object"}),
        }
    }
    async fn execute(&self, _args: Value) -> Result<Value> {
        self.started.add_permits(1);
        let _permit = self.release.acquire().await.expect("open");
        Ok(json!({"finished": true}))
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
            parameters: json!({"type": "object"}),
        }
    }
    async fn execute(&self, _args: Value) -> Result<Value> {
        std::future::pending().await
    }
}

fn probe() -> (Arc<dyn Tool>, Arc<AtomicUsize>) {
    let runs = Arc::new(AtomicUsize::new(0));
    (Arc::new(Probe { runs: runs.clone() }), runs)
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

fn spec(item: &str, tools: Vec<Arc<dyn Tool>>) -> GrantSpec {
    GrantSpec {
        item: item.into(),
        conversation: Uuid::new_v4(),
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
    let spec = spec("item-a", vec![tool]);
    let conversation = spec.conversation;
    let grant = grants.mint(spec);
    let token = grant.token().expose().to_string();
    // The agent names another item, conversation and actor; the grant's
    // are used.
    let forged = json!({"item": "item-b", "conversation": Uuid::new_v4(), "actor": "user:master"});
    let answer = grants
        .call(&token, "s:1", "probe", forged.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(answer["bound"], json!(["item-a", "worker:pinch"]));
    assert_eq!(answer["conversation"], json!(conversation));
    assert_eq!(answer["args"], forged);
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    assert_eq!(
        outcomes(&grant.finish(None)),
        vec![("probe", CallOutcome::Ok)]
    );
}

/// Through the real work tools: scope and conversation from the grant, a
/// refusal is a failure for the record and the agent alike, and a question
/// that parks the item ends the run as it would on the host half.
#[tokio::test]
async fn the_work_tools_take_the_grants_scope_and_a_parked_question_ends_the_run() {
    let backend = Arc::new(StubWorkBackend::new());
    let grants = Arc::new(McpGrants::new());
    let tools = work_tools(backend.clone());
    let mut names: Vec<&str> = tools.iter().map(|tool| tool.name()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec!["ask_user", "capability_request", "work_file", "work_status"]
    );
    let spec = spec("item-a", tools);
    let conversation = spec.conversation.to_string();
    let grant = grants.mint(spec);
    let token = grant.token().expose().to_string();
    grants
        .call(&token, "s:1", "work_status", json!({"ids": ["item-b"]}))
        .await
        .unwrap()
        .unwrap();
    grants
        .call(
            &token,
            "s:2",
            "work_file",
            json!({"title": "Follow up", "objective": "Check the calendar", "done_when": "Checked"}),
        )
        .await
        .unwrap()
        .unwrap();
    // An item named in the arguments is refused by the tool's own checks,
    // and the agent is told it failed.
    let rejected = grants
        .call(
            &token,
            "s:3",
            "ask_user",
            json!({"text": "Which calendar?", "item": "item-b"}),
        )
        .await
        .unwrap()
        .unwrap_err();
    assert!(rejected.contains("rejected"), "{rejected}");
    let parked = grants
        .call(
            &token,
            "s:4",
            "ask_user",
            json!({"text": "Which calendar?"}),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(parked["parked"], true);
    assert_eq!(parked["next"], RUN_ENDED);
    // The minter learns of it, and nothing more is admitted.
    let summary = tokio::time::timeout(Duration::from_secs(1), grant.ended())
        .await
        .unwrap();
    assert!(!summary.is_empty());
    assert_eq!(
        grants
            .call(&token, "s:5", "work_status", json!({"ids": ["item-a"]}))
            .await,
        Err(CallError::Ended)
    );
    let calls = backend.calls();
    assert_eq!(calls.len(), 3, "{calls:?}");
    assert!(matches!(&calls[0], WorkCall::Status { principal, .. }
        if principal.item.as_deref() == Some("item-a")
            && principal.conversation_id.as_deref() == Some(conversation.as_str())));
    assert!(matches!(&calls[1], WorkCall::File { provenance, .. }
        if provenance.conversation_id.as_deref() == Some(conversation.as_str())
            && provenance.filed_by_item.as_deref() == Some("item-a")));
    assert!(matches!(&calls[2], WorkCall::Ask { item, provenance, .. }
        if item == "item-a" && provenance.actor == "worker:pinch"));
    let record = grant.finish(None);
    assert_eq!(record.ended, Some(summary));
    assert_eq!(
        outcomes(&record),
        vec![
            ("work_status", CallOutcome::Ok),
            ("work_file", CallOutcome::Ok),
            ("ask_user", CallOutcome::Failed),
            ("ask_user", CallOutcome::Ok),
        ]
    );
}

#[tokio::test]
async fn a_call_outside_the_grant_is_refused_and_recorded_as_denied() {
    let grants = Arc::new(McpGrants::new());
    let (tool, runs) = probe();
    let grant = grants.mint(spec("item-a", vec![tool]));
    let token = grant.token().expose().to_string();
    assert_eq!(
        grants
            .call(&token, "s:1", "exec", json!({"command": "id"}))
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
        outcomes(&grant.finish(None)),
        vec![("exec", CallOutcome::Denied)]
    );
}

#[tokio::test]
async fn a_read_only_review_is_granted_no_tools() {
    let grants = Arc::new(McpGrants::new());
    let (tool, runs) = probe();
    let grant = grants.mint(GrantSpec {
        read_only: true,
        ..spec("review", vec![tool])
    });
    let token = grant.token().expose().to_string();
    assert!(grants.list(&token).unwrap().is_empty());
    assert_eq!(
        grants.call(&token, "s:1", "probe", json!({})).await,
        Err(CallError::NotGranted)
    );
    assert_eq!(runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn every_attempt_counts_against_the_quota() {
    let grants = Arc::new(McpGrants::new());
    let (tool, runs) = probe();
    let grant = grants.mint(GrantSpec {
        quota: 2,
        ..spec("item-a", vec![tool])
    });
    let token = grant.token().expose().to_string();
    assert!(grants.call(&token, "s:1", "probe", json!({})).await.is_ok());
    // A refused attempt counts too, and a new session is the same run.
    assert_eq!(
        grants.call(&token, "s:2", "exec", json!({})).await,
        Err(CallError::NotGranted)
    );
    assert_eq!(
        grants.call(&token, "t:1", "probe", json!({})).await,
        Err(CallError::QuotaSpent)
    );
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_request_id_is_used_once() {
    let grants = Arc::new(McpGrants::new());
    let (tool, runs) = probe();
    let grant = grants.mint(spec("item-a", vec![tool]));
    let token = grant.token().expose().to_string();
    assert!(grants
        .call(&token, "s:1", "probe", json!({"n": 1}))
        .await
        .is_ok());
    // Repeated, with the same payload or another: refused, never run again.
    for args in [json!({"n": 1}), json!({"n": 2})] {
        assert_eq!(
            grants.call(&token, "s:1", "probe", args).await,
            Err(CallError::RepeatedId)
        );
    }
    // The same JSON-RPC id from another bridge session is another call.
    assert!(grants
        .call(&token, "t:1", "probe", json!({"n": 1}))
        .await
        .is_ok());
    assert_eq!(runs.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn one_call_is_in_flight_per_grant() {
    let grants = Arc::new(McpGrants::new());
    let (tool, started, release) = gate();
    let (other, _) = probe();
    let grant = grants.mint(spec("item-a", vec![tool, other]));
    let token = grant.token().expose().to_string();
    let first = tokio::spawn({
        let (grants, token) = (grants.clone(), token.clone());
        async move { grants.call(&token, "s:1", "gate", json!({})).await }
    });
    started.acquire().await.unwrap().forget();
    for (id, tool) in [("s:2", "probe"), ("s:3", "gate")] {
        assert_eq!(
            grants.call(&token, id, tool, json!({})).await,
            Err(CallError::Busy),
            "{id} {tool}"
        );
    }
    release.add_permits(1);
    assert_eq!(first.await.unwrap(), Ok(Ok(json!({"finished": true}))));
    assert!(grants.call(&token, "s:4", "probe", json!({})).await.is_ok());
}

#[tokio::test]
async fn revocation_stops_admission_and_cancels_the_call_in_flight() {
    let grants = Arc::new(McpGrants::new());
    let (tool, started, _release) = gate();
    let grant = grants.mint(spec("item-a", vec![tool]));
    let token = grant.token().expose().to_string();
    let in_flight = tokio::spawn({
        let (grants, token) = (grants.clone(), token.clone());
        async move { grants.call(&token, "s:1", "gate", json!({})).await }
    });
    started.acquire().await.unwrap().forget();
    let record = grant.finish(None);
    assert_eq!(outcomes(&record), vec![("gate", CallOutcome::Ambiguous)]);
    assert_eq!(in_flight.await.unwrap(), Err(CallError::Cancelled));
    assert!(!grants.is_live(&token));
    assert_eq!(grants.initialize(&token), Err(CallError::NotLive));
    assert_eq!(
        grants.call(&token, "s:2", "gate", json!({})).await,
        Err(CallError::NotLive)
    );
}

#[tokio::test]
async fn a_dropped_grant_is_revoked() {
    let grants = Arc::new(McpGrants::new());
    let grant = grants.mint(spec("item-a", vec![]));
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
        ..spec("item-a", vec![Arc::new(Hang)])
    });
    let token = grant.token().expose().to_string();
    // The tool's own cap is 30 s; the grant's deadline comes first.
    assert_eq!(
        grants.call(&token, "s:1", "hang", json!({})).await,
        Err(CallError::Cancelled)
    );
    assert!(!grants.is_live(&token));
    assert_eq!(
        grants.call(&token, "s:2", "hang", json!({})).await,
        Err(CallError::NotLive)
    );
    assert_eq!(
        outcomes(&grant.finish(None)),
        vec![("hang", CallOutcome::Ambiguous)]
    );
}

#[tokio::test(start_paused = true)]
async fn a_call_is_clamped_to_the_tools_own_cap() {
    let grants = Arc::new(McpGrants::new());
    let grant = grants.mint(spec("item-a", vec![Arc::new(Hang)]));
    let token = grant.token().expose().to_string();
    let started = Instant::now();
    let answer = grants.call(&token, "s:1", "hang", json!({})).await.unwrap();
    assert_eq!(
        started.elapsed(),
        Duration::from_secs(tool_cap_secs("hang", &Default::default()))
    );
    assert!(answer
        .unwrap_err()
        .contains("whether it took effect is unknown"));
    // The grant outlives the call.
    assert!(grants.is_live(&token));
    assert_eq!(
        outcomes(&grant.finish(None)),
        vec![("hang", CallOutcome::Ambiguous)]
    );
}

/// Each grant attests only its own run's report: two runs on one worker at
/// once keep their own calls, and an old run's late result reaches neither
/// its own sealed record nor the next run's.
#[tokio::test]
async fn two_concurrent_runs_on_one_worker_keep_their_own_records() {
    let grants = Arc::new(McpGrants::new());
    let (tool, _) = probe();
    let a = grants.mint(spec("item-a", vec![tool.clone()]));
    let b = grants.mint(spec("item-b", vec![tool]));
    let (token_a, token_b) = (
        a.token().expose().to_string(),
        b.token().expose().to_string(),
    );
    assert_ne!(token_a, token_b);
    let (answer_a, answer_b, denied_b) = tokio::join!(
        grants.call(&token_a, "s:1", "probe", json!({})),
        grants.call(&token_b, "s:1", "probe", json!({})),
        grants.call(&token_b, "s:2", "exec", json!({})),
    );
    assert_eq!(
        answer_a.unwrap().unwrap()["bound"],
        json!(["item-a", "worker:pinch"])
    );
    assert_eq!(
        answer_b.unwrap().unwrap()["bound"],
        json!(["item-b", "worker:pinch"])
    );
    assert_eq!(denied_b, Err(CallError::NotGranted));
    let (mut report_a, mut report_b) = (ResultReport::default(), ResultReport::default());
    a.finish(Some(&mut report_a));
    b.finish(Some(&mut report_b));
    let attested = |report: &ResultReport| -> Vec<String> {
        report.artifacts.iter().map(|a| a.value.clone()).collect()
    };
    assert_eq!(
        attested(&report_a),
        vec![r#"{"tool":"probe","outcome":"ok"}"#]
    );
    assert_eq!(
        attested(&report_b),
        vec![
            r#"{"tool":"probe","outcome":"ok"}"#,
            r#"{"tool":"exec","outcome":"denied"}"#
        ]
    );
}

#[tokio::test]
async fn a_late_result_from_an_old_run_reaches_no_record() {
    let grants = Arc::new(McpGrants::new());
    let (tool, started, release) = gate();
    // The controller may give the item's next run the same run id
    // (a scheduled job's): only the grant tells the runs apart.
    let old = grants.mint(spec("item-a", vec![tool.clone()]));
    let old_token = old.token().expose().to_string();
    let late = tokio::spawn({
        let (grants, token) = (grants.clone(), old_token.clone());
        async move { grants.call(&token, "s:1", "gate", json!({})).await }
    });
    started.acquire().await.unwrap().forget();
    // The old run ends; the item runs again under a new grant.
    let old_live = old.live.clone();
    let old_record = old.finish(None);
    let new = grants.mint(spec("item-a", vec![tool]));
    release.add_permits(1);
    assert_eq!(late.await.unwrap(), Err(CallError::Cancelled));
    assert_eq!(
        outcomes(&old_record),
        vec![("gate", CallOutcome::Ambiguous)]
    );
    // A result that still arrives for the old call is dropped: the old
    // record stays as it was sealed.
    assert!(!old_live.settle("gate", CallOutcome::Ok, None));
    assert_eq!(old_live.seal(), old_record);
    let mut report = ResultReport::default();
    let new_record = new.finish(Some(&mut report));
    assert!(new_record.calls.is_empty(), "{new_record:?}");
    assert!(report.artifacts.is_empty());
}

#[tokio::test]
async fn finishing_attests_the_report_in_place_of_the_agents_own_claims() {
    let grants = Arc::new(McpGrants::new());
    let (tool, _) = probe();
    let grant = grants.mint(spec("item-a", vec![tool]));
    let token = grant.token().expose().to_string();
    grants
        .call(&token, "s:1", "probe", json!({}))
        .await
        .unwrap()
        .unwrap();
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
    grant.finish(Some(&mut report));
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
    let initialized_only = grants.mint(spec("item-a", vec![]));
    grants
        .initialize(initialized_only.token().expose())
        .unwrap();
    assert_eq!(initialized_only.finish(None).bridge_connected, None);
    let connected = grants.mint(spec("item-a", vec![]));
    let token = connected.token().expose().to_string();
    grants.initialize(&token).unwrap();
    grants.list(&token).unwrap();
    let record = connected.finish(None);
    assert!(record.bridge_connected.is_some());
    // Reachability is never a call.
    assert!(record.calls.is_empty());
}

#[test]
fn tokens_are_random_prefixed_and_never_printed() {
    let grants = Arc::new(McpGrants::new());
    let a = grants.mint(spec("item-a", vec![]));
    let b = grants.mint(spec("item-a", vec![]));
    assert!(a.token().expose().starts_with(TOKEN_PREFIX));
    assert_eq!(a.token().expose().len(), TOKEN_PREFIX.len() + 64);
    assert_ne!(a.token(), b.token());
    assert!(!format!("{:?}", a.token()).contains(a.token().expose()));
}
