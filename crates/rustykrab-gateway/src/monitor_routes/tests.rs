use super::*;
use async_trait::async_trait;
use chrono::TimeDelta;
use rustykrab_control::{
    controller::{Controller, ControllerConfig},
    handle::ControlHandle,
};
use rustykrab_core::{
    model::{ModelProvider, ModelResponse},
    types::{Message, ToolSchema},
    Error,
};
use std::{net::SocketAddr, sync::Arc};

struct Unused;
#[async_trait]
impl ModelProvider for Unused {
    fn name(&self) -> &str {
        "unused"
    }
    async fn chat(&self, _: &[Message], _: &[ToolSchema]) -> rustykrab_core::Result<ModelResponse> {
        Err(Error::Internal(
            "no model should run during observation".into(),
        ))
    }
}

struct ResourceFixture;
impl crate::resources::ResourceObserver for ResourceFixture {
    fn services(&self) -> Vec<crate::resources::ServiceObservation> {
        vec![]
    }
    fn registered(&self, id: &str) -> bool {
        id == "fixture"
    }
}

async fn serve() -> (
    String,
    reqwest::Client,
    rustykrab_store::Store,
    Arc<Controller>,
    tokio::task::JoinHandle<()>,
) {
    serve_mode(false).await
}

async fn serve_mode(
    manager: bool,
) -> (
    String,
    reqwest::Client,
    rustykrab_store::Store,
    Arc<Controller>,
    tokio::task::JoinHandle<()>,
) {
    let dir = std::env::temp_dir().join(format!("rk-monitor-{}", uuid::Uuid::new_v4()));
    let store = rustykrab_store::Store::open(&dir, vec![7; 32]).unwrap();
    let control = Arc::new(Controller::new(
        store.clone(),
        vec![],
        ControllerConfig::default(),
    ));
    control.tick().await.unwrap();
    let mut state = AppState::new(
        store.clone(),
        vec![],
        Arc::new(Unused),
        "test-monitor-token".into(),
    )
    .with_control(control.clone());
    state.agent.work_manager = manager;
    if manager {
        state.resources = Some(Arc::new(ResourceFixture));
    }
    let app = crate::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap()
    });
    (base, reqwest::Client::new(), store, control, task)
}

#[tokio::test]
async fn real_router_enforces_auth_origin_and_observation_is_read_only() {
    let (base, client, store, control, task) = serve().await;
    let path = format!("{base}/api/monitor");
    let before = control.loop_status().unwrap();
    assert_eq!(
        client
            .get(&path)
            .header("Origin", &base)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .get(&path)
            .bearer_auth("test-monitor-token")
            .header("Origin", "https://outside.example")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let browser_read = client
        .get(&path)
        .bearer_auth("test-monitor-token")
        .header("Sec-Fetch-Site", "same-origin")
        .header("Sec-Fetch-Mode", "cors")
        .header("Sec-Fetch-Dest", "empty")
        .header("Referer", format!("{base}/monitor.html"));
    assert_eq!(
        browser_read
            .try_clone()
            .unwrap()
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        browser_read
            .header("Origin", "https://outside.example")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        client
            .post(format!("{base}/api/work/tick"))
            .bearer_auth("test-monitor-token")
            .header("Sec-Fetch-Site", "same-origin")
            .header("Sec-Fetch-Mode", "cors")
            .header("Sec-Fetch-Dest", "empty")
            .header("Referer", format!("{base}/monitor.html"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN,
        "commands retain mandatory Origin"
    );
    let response = client
        .get(&path)
        .bearer_auth("test-monitor-token")
        .header("Origin", &base)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let reply: MonitorReply = response.json().await.unwrap();
    assert_eq!(reply.health, "healthy");
    assert_eq!(reply.work.total_live, 0);
    assert_eq!(
        control.loop_status().unwrap(),
        before,
        "a monitoring read must not tick"
    );
    assert_eq!(store.work_events_last_id().await.unwrap(), 0);
    let metrics = client
        .get(format!("{base}/api/monitor/metrics"))
        .bearer_auth("test-monitor-token")
        .header("Origin", &base)
        .send()
        .await
        .unwrap();
    assert_eq!(metrics.status(), StatusCode::OK);
    assert!(metrics.headers()["content-type"]
        .to_str()
        .unwrap()
        .contains("version=0.0.4"));
    assert!(metrics
        .text()
        .await
        .unwrap()
        .contains("rustykrab_monitor_healthy 1"));
    let bad = client
        .get(format!("{path}?limit=not-a-number"))
        .bearer_auth("test-monitor-token")
        .header("Origin", &base)
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    task.abort();
}

#[tokio::test]
async fn failures_stale_ticks_and_expected_cutover_waits_have_distinct_verdicts() {
    let (_, _, store, _, task) = serve().await;
    let work = store.work_monitor_snapshot(1, 1).await.unwrap();
    let now = Utc::now();
    let mut status = LoopStatus {
        last_tick: Some(now - TimeDelta::seconds(STALE_SECONDS + 1)),
        ..Default::default()
    };
    let alerts = assess(Some(&status), &[], &work, now);
    assert_eq!(health_of(&alerts), "critical");
    assert!(alerts.iter().any(|a| a.code == "controller_stale"));
    status.lock = Some(LockState::Waiting);
    assert_eq!(
        health_of(&assess(Some(&status), &[], &work, now)),
        "healthy"
    );
    status.consecutive_failed_ticks = 2;
    status.last_failure_class = Some("storage".into());
    assert!(assess(Some(&status), &[], &work, now)
        .iter()
        .any(|a| a.code == "controller_failing" && a.message.contains("storage")));
    assert_eq!(health_of(&assess(None, &[], &work, now)), "critical");
    task.abort();
}

#[test]
fn exposition_label_escaping_keeps_each_sample_on_one_line() {
    assert_eq!(label("worker\\one\"\ntwo"), "worker\\\\one\\\"\\ntwo");
}

#[tokio::test]
async fn lease_loss_wall_budget_and_expected_questions_are_reported_without_claiming_failure() {
    use rustykrab_core::work::{Lease, PlanOutcome, Status, WorkItemDraft};
    let (_, _, store, control, task) = serve().await;
    let outcome = control
        .file_draft(
            WorkItemDraft {
                title: "Observe execution".into(),
                objective: "o".into(),
                done_when: "d".into(),
                ..Default::default()
            },
            rustykrab_control::Provenance {
                conversation_id: None,
                filed_by_item: None,
                actor: "user:test".into(),
            },
        )
        .await
        .unwrap();
    assert!(matches!(outcome, PlanOutcome::Accepted(_)));
    let mut work = store.work_monitor_snapshot(20, 20).await.unwrap();
    let now = Utc::now();
    let row = &mut work.items[0];
    row.item.status = Status::Running;
    row.item.budget.wall_seconds = 60;
    row.lease = Some(Lease {
        item: row.item.id.clone(),
        worker: "pinch".into(),
        since: now - TimeDelta::seconds(100),
        heartbeat_at: now - TimeDelta::seconds(91),
        ttl_seconds: 90,
        inputs: vec![],
    });
    let alerts = assess(control.loop_status().as_ref(), &[], &work, now);
    assert!(alerts
        .iter()
        .any(|a| a.code == "lease_expired" && a.worker.as_deref() == Some("pinch")));
    assert!(alerts.iter().any(|a| a.code == "run_over_budget"));
    work.items[0].lease = None;
    assert!(assess(control.loop_status().as_ref(), &[], &work, now)
        .iter()
        .any(|a| a.code == "active_without_lease"));
    work.items[0].children = 2; // A running parent is a roll-up, never a lease.
    work.pending_questions = 1;
    work.pending_notices = 1;
    work.oldest_pending_notice = Some(now + TimeDelta::minutes(20));
    let alerts = assess(control.loop_status().as_ref(), &[], &work, now);
    assert_eq!(
        health_of(&alerts),
        "healthy",
        "a planned wait or delayed digest is not an execution fault"
    );
    assert!(alerts.iter().any(|a| a.code == "questions_waiting"));
    work.oldest_pending_notice = Some(now - TimeDelta::seconds(301));
    assert!(assess(control.loop_status().as_ref(), &[], &work, now)
        .iter()
        .any(|a| a.code == "notice_delivery_delayed"));
    task.abort();
}

#[tokio::test]
async fn old_or_absent_worker_checks_do_not_read_as_current_health() {
    let (_, _, store, _, task) = serve().await;
    let work = store.work_monitor_snapshot(1, 1).await.unwrap();
    let now = Utc::now();
    let status = LoopStatus {
        last_tick: Some(now),
        ..Default::default()
    };
    let mut worker = WorkerView {
        name: "observer".into(),
        kind: "local".into(),
        live: true,
        healthy: true,
        health: "healthy".into(),
        last_seen: Some(now),
        cost_tier: 0,
        concurrency: 1,
        capabilities: Default::default(),
        routing_record: Default::default(),
        spec: None,
        runtime: None,
        created_at: now,
    };
    assert_eq!(
        health_of(&assess(Some(&status), &[worker.clone()], &work, now)),
        "healthy"
    );
    for last_seen in [None, Some(now - TimeDelta::seconds(STALE_SECONDS + 1))] {
        worker.last_seen = last_seen;
        let alerts = assess(Some(&status), &[worker.clone()], &work, now);
        assert_eq!(health_of(&alerts), "degraded");
        assert!(alerts.iter().any(|a| a.code == "worker_check_stale"));
    }
    task.abort();
}

#[tokio::test]
async fn manager_schedule_and_service_commands_share_auth_origin_and_durable_queue() {
    let (base, client, store, control, task) = serve_mode(true).await;
    let schedule = format!("{base}/api/schedules");
    assert_eq!(
        client
            .get(&schedule)
            .header("Origin", &base)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let body = json!({"schedule":"0 9 * * *","task":"Perform the scheduled task","timezone":"America/Los_Angeles","execution":{"kind":"research","worker_kind":"codex"}});
    assert_eq!(
        client
            .post(&schedule)
            .bearer_auth("test-monitor-token")
            .header("Origin", "https://outside.example")
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let r = client
        .post(&schedule)
        .bearer_auth("test-monitor-token")
        .header("Origin", &base)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    let job: rustykrab_store::ScheduledJob = r.json().await.unwrap();
    assert_eq!(
        job.execution.as_ref().unwrap().worker_kind,
        rustykrab_core::work::WorkerKind::Codex
    );
    assert!(
        store.jobs().work_item_id(&job.id).await.unwrap().is_none(),
        "Creating a schedule must not immediately execute it"
    );
    let before = control.loop_status().unwrap();
    let r = client
        .get(format!("{schedule}/{}", job.id))
        .bearer_auth("test-monitor-token")
        .header("Origin", &base)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(control.loop_status().unwrap(), before);
    let r = client
        .post(format!("{schedule}/{}/enabled", job.id))
        .bearer_auth("test-monitor-token")
        .header("Origin", &base)
        .json(&json!({"enabled":false}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert!(!store.jobs().get_job(&job.id).await.unwrap().enabled);
    let actions = format!("{base}/api/resources/fixture/actions");
    assert_eq!(
        client
            .post(&actions)
            .bearer_auth("test-monitor-token")
            .json(&json!({"action":"restart"}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let r = client
        .post(&actions)
        .bearer_auth("test-monitor-token")
        .header("Origin", &base)
        .json(&json!({"action":"ensure_running"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    let receipt: rustykrab_core::work::PlanOutcome = r.json().await.unwrap();
    let rustykrab_core::work::PlanOutcome::Accepted(a) = receipt else {
        panic!("refused")
    };
    let saved = store.work_get(&a.root).await.unwrap().unwrap();
    assert!(!saved.status.is_closed());
    assert!(store.work_lease_history(&a.root).await.unwrap().is_empty());
    assert_eq!(
        client
            .post(&actions)
            .bearer_auth("test-monitor-token")
            .header("Origin", &base)
            .json(&json!({"action":"shell","command":"touch /tmp/file"}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    task.abort();
}
