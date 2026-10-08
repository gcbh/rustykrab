//! Observations of agents and their work, behind the existing auth/origin boundary.
//! Reading these routes never ticks the controller, refreshes a worker, or alters work.
use crate::AppState;
use axum::{
    extract::{Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use chrono::{DateTime, Utc};
use rustykrab_control::{
    handle::{LockState, LoopStatus},
    registry::WorkerView,
};
use rustykrab_core::proposal::MetricValue;
use rustykrab_store::WorkMonitorSnapshot;
use serde::{Deserialize, Serialize};
use serde_json::json;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/monitor", get(snapshot))
        .route("/api/monitor/metrics", get(metrics))
}

#[derive(Debug, Deserialize)]
pub struct MonitorQuery {
    #[serde(default = "item_limit")]
    pub limit: usize,
    #[serde(default = "event_limit")]
    pub events: usize,
}
fn item_limit() -> usize {
    200
}
fn event_limit() -> usize {
    50
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alert {
    pub severity: String,
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MonitorReply {
    #[serde(default)]
    pub work_manager: bool,
    #[serde(default)]
    pub services: Vec<crate::resources::ServiceObservation>,
    #[serde(default)]
    pub schedules: Vec<rustykrab_store::ScheduledJob>,
    pub version: String,
    pub commit: Option<String>,
    pub controller: Option<LoopStatus>,
    pub workers: Vec<WorkerView>,
    pub work: WorkMonitorSnapshot,
    pub expectation_metrics: Vec<MetricValue>,
    #[serde(default)]
    pub dreaming: Option<rustykrab_core::dream_review::DreamingView>,
    /// Healthy, degraded (warnings), or critical. Expected waits are informational.
    pub health: String,
    pub alerts: Vec<Alert>,
    pub stale_after_seconds: i64,
}

fn alert(
    severity: &str,
    code: &str,
    message: impl Into<String>,
    item: Option<&str>,
    worker: Option<&str>,
) -> Alert {
    Alert {
        severity: severity.into(),
        code: code.into(),
        message: message.into(),
        item: item.map(str::to_owned),
        worker: worker.map(str::to_owned),
    }
}

const STALE_SECONDS: i64 = 120;

/// Deterministic classification of observations; never a worker's prose verdict.
fn assess(
    controller: Option<&LoopStatus>,
    workers: &[WorkerView],
    work: &WorkMonitorSnapshot,
    now: DateTime<Utc>,
) -> Vec<Alert> {
    let mut out = Vec::new();
    match controller {
        None => out.push(alert(
            "critical",
            "controller_unobserved",
            "The controller has no observable loop status.",
            None,
            None,
        )),
        Some(s) => {
            if s.consecutive_failed_ticks > 0 {
                out.push(alert(
                    "critical",
                    "controller_failing",
                    format!(
                        "{} consecutive controller ticks failed ({}).",
                        s.consecutive_failed_ticks,
                        s.last_failure_class.as_deref().unwrap_or("unknown")
                    ),
                    None,
                    None,
                ));
            }
            if s.draining {
                out.push(alert(
                    "info",
                    "controller_draining",
                    "The controller is draining and leases no new work.",
                    None,
                    None,
                ));
            } else if s.lock == Some(LockState::Waiting) {
                out.push(alert(
                    "info",
                    "controller_waiting",
                    "Another daemon holds the controller lock.",
                    None,
                    None,
                ));
            } else {
                match s.last_tick {
                    Some(at) if (now - at).num_seconds() > STALE_SECONDS => out.push(alert(
                        "critical",
                        "controller_stale",
                        format!(
                            "The last completed tick was {} seconds ago.",
                            (now - at).num_seconds()
                        ),
                        None,
                        None,
                    )),
                    None => out.push(alert(
                        "warning",
                        "controller_starting",
                        "The controller has not completed its first tick.",
                        None,
                        None,
                    )),
                    _ => {}
                }
            }
        }
    }
    for w in workers {
        if !w.live || !w.healthy {
            out.push(alert(
                "warning",
                "worker_unavailable",
                format!("{}: {}", w.name, w.health),
                None,
                Some(&w.name),
            ));
        } else if w
            .last_seen
            .is_none_or(|at| (now - at).num_seconds() > STALE_SECONDS)
        {
            out.push(alert(
                "warning",
                "worker_check_stale",
                format!("{} has no recent worker health check.", w.name),
                None,
                Some(&w.name),
            ));
        }
    }
    let open: u64 = work
        .counts
        .iter()
        .filter(|(s, _)| !matches!(s.as_str(), "done" | "failed" | "cancelled" | "expired"))
        .map(|(_, count)| count)
        .sum();
    if open > work.items.len() as u64 {
        out.push(alert(
            "warning",
            "monitor_coverage_limited",
            format!(
                "{} open items exceed this snapshot's {} displayed rows.",
                open,
                work.items.len()
            ),
            None,
            None,
        ));
    }
    for row in &work.items {
        let i = &row.item;
        if let Some(lease) = &row.lease {
            let since_beat = (now - lease.heartbeat_at).num_seconds().max(0) as u64;
            if since_beat > lease.ttl_seconds {
                out.push(alert(
                    "critical",
                    "lease_expired",
                    format!(
                        "{} has missed its lease heartbeat for {} seconds.",
                        i.title, since_beat
                    ),
                    Some(&i.id),
                    Some(&lease.worker),
                ));
            }
            if i.budget.wall_seconds > 0
                && (now - lease.since).num_seconds().max(0) as u64 > i.budget.wall_seconds
            {
                out.push(alert(
                    "critical",
                    "run_over_budget",
                    format!("{} is past its wall-time budget.", i.title),
                    Some(&i.id),
                    Some(&lease.worker),
                ));
            }
        } else if row.children == 0 && i.status.is_active() {
            out.push(alert(
                "critical",
                "active_without_lease",
                format!("{} is active with no durable lease.", i.title),
                Some(&i.id),
                None,
            ));
        }
        if i.status.name() == "blocked" && (now - i.updated_at).num_hours() >= 24 {
            out.push(alert(
                "info",
                "long_blocked",
                format!(
                    "{} has been blocked for at least a day ({}).",
                    i.title,
                    i.status.reason().unwrap_or("unknown")
                ),
                Some(&i.id),
                None,
            ));
        }
    }
    if work.pending_questions > 0 {
        out.push(alert(
            "info",
            "questions_waiting",
            format!("{} questions await an answer.", work.pending_questions),
            None,
            None,
        ));
    }
    if work
        .oldest_pending_notice
        .is_some_and(|at| (now - at).num_seconds() > 300)
    {
        out.push(alert(
            "warning",
            "notice_delivery_delayed",
            format!(
                "{} work notices remain undelivered; the oldest is over five minutes old.",
                work.pending_notices
            ),
            None,
            None,
        ));
    }
    out
}

pub fn health_of(alerts: &[Alert]) -> &'static str {
    if alerts.iter().any(|a| a.severity == "critical") {
        "critical"
    } else if alerts.iter().any(|a| a.severity == "warning") {
        "degraded"
    } else {
        "healthy"
    }
}

async fn observe(state: &AppState, query: MonitorQuery) -> Result<MonitorReply, String> {
    let work = state
        .agent
        .store
        .work_monitor_snapshot(query.limit, query.events)
        .await
        .map_err(|e| e.to_string())?;
    let workers = match &state.workers {
        Some(registry) => registry.views().await.map_err(|e| e.to_string())?,
        None => Vec::new(),
    };
    let controller = state.control.as_ref().and_then(|c| c.loop_status());
    let mut alerts = assess(controller.as_ref(), &workers, &work, Utc::now());
    let dreaming = match state.evaluation.as_ref() {
        Some(h) => h.dreaming_status().await.ok(),
        None => None,
    };
    if let Some(d) = &dreaming {
        if let Some(r) = d
            .reviews
            .first()
            .filter(|r| r.stage == rustykrab_core::dream_review::ReviewStage::Failed)
        {
            alerts.push(alert(
                "warning",
                "dreaming_review_failed",
                "The latest project dreaming review failed; inspect its receipt.",
                r.generator_item.as_deref(),
                None,
            ));
        }
        if d.reviews.iter().any(|r| {
            !r.stage.terminal()
                && work.items.iter().any(|w| {
                    r.generator_item
                        .iter()
                        .chain(&r.evaluator_item)
                        .any(|id| id == &w.item.id)
                        && !w.item.status.is_closed()
                        && Utc::now() - w.item.created_at > chrono::Duration::minutes(30)
                })
        }) {
            alerts.push(alert(
                "warning",
                "dreaming_stalled",
                "A native dreaming job is past its pending-job deadline.",
                None,
                None,
            ));
        }
    }
    let services = state
        .resources
        .as_ref()
        .map(|r| r.services())
        .unwrap_or_default();
    for service in &services {
        let stale = service
            .checked_at
            .is_none_or(|t| Utc::now() - t > chrono::Duration::seconds(STALE_SECONDS));
        if service.healthy != Some(true) || !service.supervised || stale {
            alerts.push(alert(
                "warning",
                "service_unhealthy",
                format!(
                    "Service {}: {}{}",
                    service.id,
                    service.detail,
                    if stale {
                        " (observation stale or absent)"
                    } else {
                        ""
                    }
                ),
                None,
                None,
            ));
        }
    }
    let health = health_of(&alerts).into();
    Ok(MonitorReply {
        work_manager: state.agent.work_manager,
        services,
        schedules: state
            .agent
            .store
            .jobs()
            .list_jobs()
            .await
            .map_err(|e| e.to_string())?,
        version: state.build.version.clone(),
        commit: state.build.commit.clone(),
        controller,
        workers,
        work,
        alerts,
        health,
        stale_after_seconds: STALE_SECONDS,
        dreaming,
        expectation_metrics: state
            .agent
            .store
            .expectation_metrics_latest()
            .await
            .map_err(|e| e.to_string())?,
    })
}

fn failure(error: String) -> Response {
    tracing::warn!(%error, "monitor observation failed");
    (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": "monitor_unavailable", "message": "Monitoring could not read the durable work state."}))).into_response()
}

async fn snapshot(State(state): State<AppState>, Query(query): Query<MonitorQuery>) -> Response {
    match observe(&state, query).await {
        Ok(reply) => ([(header::CACHE_CONTROL, "no-store")], Json(reply)).into_response(),
        Err(e) => failure(e),
    }
}

fn label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// Prometheus exposition, authenticated exactly like the JSON endpoint.
/// Every count describes durable state; token/wall totals are finalized runs only.
fn exposition(reply: &MonitorReply) -> String {
    let mut s = String::from("# HELP rustykrab_monitor_healthy Whether monitoring found no warning or critical condition.\n# TYPE rustykrab_monitor_healthy gauge\n");
    s.push_str(&format!(
        "rustykrab_monitor_healthy {}\n",
        u8::from(reply.health == "healthy")
    ));
    if let Some(d) = &reply.dreaming {
        s.push_str(&format!("rustykrab_dreaming_enabled {}\nrustykrab_dreaming_reviews_completed {}\nrustykrab_dreaming_reviews_failed {}\nrustykrab_dreaming_reviews_pending {}\nrustykrab_dreaming_projects_eligible {}\nrustykrab_dreaming_projects_reviewed {}\nrustykrab_dreaming_proposals {}\nrustykrab_dreaming_outcomes_measured {}\nrustykrab_dreaming_tokens {}\n",u8::from(d.enabled),d.metrics.completed,d.metrics.failed,d.metrics.pending,d.metrics.eligible_projects,d.metrics.reviewed_projects,d.metrics.proposal_count,d.metrics.measured_outcomes,d.metrics.tokens));
    }
    s.push_str(&format!(
        "rustykrab_work_manager_enabled {}\n",
        u8::from(reply.work_manager)
    ));
    for service in &reply.services {
        let id = label(&service.id);
        s.push_str(&format!("rustykrab_service_healthy{{service=\"{id}\"}} {}\nrustykrab_service_supervised{{service=\"{id}\"}} {}\nrustykrab_service_identity_verified{{service=\"{id}\"}} {}\n", u8::from(service.healthy == Some(true)), u8::from(service.supervised), u8::from(service.identity_verified)));
        if let Some(at) = service.checked_at {
            s.push_str(&format!(
                "rustykrab_service_last_check_seconds{{service=\"{id}\"}} {}\n",
                at.timestamp()
            ));
        }
    }
    for job in &reply.schedules {
        let id = label(&job.id);
        s.push_str(&format!("rustykrab_schedule_enabled{{schedule=\"{id}\"}} {}\nrustykrab_schedule_next_run_seconds{{schedule=\"{id}\"}} {}\n", u8::from(job.enabled), job.next_run_at.timestamp()));
    }
    s.push_str("# TYPE rustykrab_work_items gauge\n");
    for (status, count) in &reply.work.counts {
        s.push_str(&format!(
            "rustykrab_work_items{{status=\"{}\"}} {}\n",
            label(status),
            count
        ));
    }
    s.push_str(&format!("rustykrab_work_archived {}\nrustykrab_questions_waiting {}\nrustykrab_notices_pending {}\n",
        reply.work.total_archived, reply.work.pending_questions, reply.work.pending_notices));
    for worker in &reply.workers {
        let name = label(&worker.name);
        let active = reply
            .work
            .active_by_worker
            .get(&worker.name)
            .copied()
            .unwrap_or(0);
        s.push_str(&format!("rustykrab_worker_healthy{{worker=\"{name}\"}} {}\nrustykrab_worker_active{{worker=\"{name}\"}} {active}\n",
            u8::from(worker.live && worker.healthy)));
        if let Some(at) = worker.last_seen {
            s.push_str(&format!(
                "rustykrab_worker_last_check_seconds{{worker=\"{name}\"}} {}\n",
                at.timestamp()
            ));
        }
        for (class, record) in &worker.routing_record {
            let class = label(class);
            for (outcome, count) in [
                ("verified", record.verified_done),
                ("unverified_claim", record.claimed_not_verified),
                ("failed", record.failed),
                ("repair", record.repairs),
            ] {
                s.push_str(&format!("rustykrab_worker_results_total{{worker=\"{name}\",class=\"{class}\",outcome=\"{outcome}\"}} {count}\n"));
            }
            s.push_str(&format!("rustykrab_worker_recorded_tokens_total{{worker=\"{name}\",class=\"{class}\"}} {}\nrustykrab_worker_recorded_wall_seconds_total{{worker=\"{name}\",class=\"{class}\"}} {}\n",
                record.cost.tokens, record.cost.wall_seconds));
        }
    }
    s.push_str(&format!(
        "rustykrab_monitor_items_truncated {}\n",
        u8::from(reply.work.items_truncated)
    ));
    let alerts: std::collections::BTreeSet<_> = reply
        .alerts
        .iter()
        .map(|a| (&a.severity, &a.code))
        .collect();
    for (severity, code) in alerts {
        s.push_str(&format!(
            "rustykrab_monitor_alert{{severity=\"{}\",code=\"{}\"}} 1\n",
            label(severity),
            label(code)
        ));
    }
    if let Some(c) = &reply.controller {
        s.push_str(&format!(
            "rustykrab_controller_runs {}\nrustykrab_controller_failed_ticks {}\n",
            c.runs_in_flight, c.consecutive_failed_ticks
        ));
        if let Some(t) = c.last_tick {
            s.push_str(&format!(
                "rustykrab_controller_last_tick_seconds {}\n",
                t.timestamp()
            ));
        }
    }
    s
}

async fn metrics(State(state): State<AppState>) -> Response {
    match observe(
        &state,
        MonitorQuery {
            limit: 500,
            events: 1,
        },
    )
    .await
    {
        Ok(reply) => (
            [
                (
                    header::CONTENT_TYPE,
                    "text/plain; version=0.0.4; charset=utf-8",
                ),
                (header::CACHE_CONTROL, "no-store"),
            ],
            exposition(&reply),
        )
            .into_response(),
        Err(e) => failure(e),
    }
}

#[cfg(test)]
mod tests;
