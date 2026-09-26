//! Route tests for `/api/work`: a real router on a loopback port, behind
//! the real auth, origin and rate-limit middleware, over a store seeded with
//! a small graph and a stub controller that records what it was asked.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use reqwest::header::{HeaderMap, HeaderValue, ORIGIN};
use reqwest::StatusCode as Http;
use serde_json::{json, Value};
use uuid::Uuid;

use rustykrab_control::graph::FilingSource;
use rustykrab_control::handle::{ControlHandle, GraphNode, GraphView, TickReport};
use rustykrab_core::model::{ModelProvider, ModelResponse};
use rustykrab_core::types::{Message, ToolSchema};
use rustykrab_core::work::{
    BlockedReason, Budget, EdgeKind, EventKind, Evidence, FailedCheck, ItemRef, PlanAccepted,
    PlanOutcome, PlanRejected, RejectionReason, Status, Trigger, WorkEvent, WorkItem, WorkItemId,
    WorkKind, WorkPlan, WorkerKind,
};
use rustykrab_core::Error;
use rustykrab_store::{Store, WorkPlanRow};
use rustykrab_tools::work_backend::Provenance;

use super::{parse_status_filter, GraphReply, StatusFilter};
use crate::AppState;

const TOKEN: &str = "work-routes-test-token";

// ── fixtures ───────────────────────────────────────────────────────────

/// A turn is never run here; the provider only has to exist.
struct UnusedProvider;

#[async_trait]
impl ModelProvider for UnusedProvider {
    fn name(&self) -> &str {
        "unused"
    }

    async fn chat(&self, _: &[Message], _: &[ToolSchema]) -> rustykrab_core::Result<ModelResponse> {
        Err(Error::ModelProvider("not used by these tests".into()))
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Call {
    FilePlan {
        actor: String,
        source: FilingSource,
        items: usize,
        origin: Option<String>,
    },
    Approve {
        root: String,
        actor: String,
    },
    Reject {
        root: String,
        reason: Option<String>,
        actor: String,
    },
    Cancel {
        item: String,
        reason: Option<String>,
        actor: String,
    },
    Tick,
}

/// Records every command and answers from the seeded store. `file_plan`
/// rejects any plan of more than three items with two failed checks, the
/// way the validator returns every failure at once.
struct StubControl {
    store: Store,
    calls: Mutex<Vec<Call>>,
}

impl StubControl {
    fn record(&self, call: Call) {
        self.calls.lock().unwrap().push(call);
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl ControlHandle for StubControl {
    async fn file_plan(
        &self,
        plan: WorkPlan,
        provenance: Provenance,
        source: FilingSource,
    ) -> Result<PlanOutcome, Error> {
        self.record(Call::FilePlan {
            actor: provenance.actor,
            source,
            items: plan.items.len(),
            origin: provenance.conversation_id,
        });
        if plan.items.len() > 3 {
            return Ok(PlanOutcome::Rejected(PlanRejected {
                failed: vec![
                    FailedCheck {
                        reason: RejectionReason::TooManyItems,
                        offending: vec![],
                        detail: format!("{} items; the cap is 3", plan.items.len()),
                    },
                    FailedCheck {
                        reason: RejectionReason::Cycle,
                        offending: vec![
                            ItemRef::Tmp { tmp: "a".into() },
                            ItemRef::Tmp { tmp: "b".into() },
                        ],
                        detail: "a blocks b blocks a".into(),
                    },
                ],
            }));
        }
        let ids = plan
            .items
            .iter()
            .filter_map(|d| d.tmp.clone())
            .map(|tmp| (tmp.clone(), format!("new-{tmp}")))
            .collect();
        Ok(PlanOutcome::Accepted(PlanAccepted {
            root: "new-a".into(),
            ids,
            held: vec![],
            policy: None,
            warnings: vec![],
        }))
    }

    async fn approve(&self, root: &str, actor: &str) -> Result<Vec<WorkItemId>, Error> {
        self.record(Call::Approve {
            root: root.into(),
            actor: actor.into(),
        });
        Ok(vec!["y".into()])
    }

    async fn reject(
        &self,
        root: &str,
        reason: Option<String>,
        actor: &str,
    ) -> Result<Vec<WorkItemId>, Error> {
        self.record(Call::Reject {
            root: root.into(),
            reason,
            actor: actor.into(),
        });
        Ok(vec!["y".into()])
    }

    async fn cancel(
        &self,
        item: &str,
        reason: Option<String>,
        actor: &str,
    ) -> Result<Vec<WorkItemId>, Error> {
        self.record(Call::Cancel {
            item: item.into(),
            reason,
            actor: actor.into(),
        });
        let subtree = self.graph(item).await?;
        Ok(subtree
            .nodes
            .iter()
            .filter(|n| !n.item.status.is_closed())
            .map(|n| n.item.id.clone())
            .collect())
    }

    async fn tick(&self) -> Result<TickReport, Error> {
        self.record(Call::Tick);
        Ok(TickReport {
            made_ready: vec!["r".into()],
            transitions: 1,
            ..TickReport::default()
        })
    }

    /// The live subtree, depth first, each parent rolled up to its own
    /// status column.
    async fn graph(&self, root: &str) -> Result<GraphView, Error> {
        let top = self
            .store
            .work_get(root)
            .await?
            .ok_or_else(|| Error::NotFound(format!("work item {root}")))?;
        let mut nodes = Vec::new();
        let mut stack = vec![(top, 0u32)];
        while let Some((item, depth)) = stack.pop() {
            let children = self.store.work_children(&item.id).await?;
            let edges = self.store.work_edges_of(&item.id).await?;
            let done = children.iter().filter(|c| c.status == Status::Done).count();
            nodes.push(GraphNode {
                rollup: (!children.is_empty()).then_some(item.status),
                children_done: done as u32,
                children_total: children.len() as u32,
                depth,
                edges,
                archived_summary: None,
                item,
            });
            for child in children.into_iter().rev() {
                stack.push((child, depth + 1));
            }
        }
        Ok(GraphView {
            root: root.into(),
            nodes,
        })
    }
}

fn at(offset: i64) -> DateTime<Utc> {
    Utc::now() - TimeDelta::days(1) + TimeDelta::seconds(offset)
}

fn item(n: i64, id: &str, kind: WorkKind, title: &str, status: Status) -> WorkItem {
    WorkItem {
        id: id.into(),
        kind,
        title: title.into(),
        objective: format!("objective of {title}"),
        done_when: "it is done".into(),
        constraints: vec![],
        decisions_made: vec![],
        artifact_refs: vec![],
        required_tools: vec![],
        required_mcp_servers: vec![],
        worker_kind: WorkerKind::Any,
        writable_resources: vec![],
        parent: None,
        inputs_from: vec![],
        origin_conversation_id: None,
        trigger: Trigger::Now,
        preconditions: vec![],
        expires_at: None,
        budget: Budget::default(),
        priority: 0,
        status,
        status_origin: None,
        plan_id: None,
        held_by: None,
        created_at: at(n),
        updated_at: at(n),
        closed_at: status.is_closed().then(|| at(n)),
    }
}

fn under(mut item: WorkItem, parent: &str) -> WorkItem {
    item.parent = Some(parent.into());
    item
}

fn cascaded(mut item: WorkItem, origin: &str) -> WorkItem {
    item.status_origin = Some(origin.into());
    item
}

fn planned(mut item: WorkItem, held: bool) -> WorkItem {
    item.plan_id = Some("plan-1".into());
    item.held_by = held.then(|| "q1".to_string());
    item
}

fn edge(item: &str, kind: EdgeKind, depends_on: &str) -> rustykrab_core::work::Edge {
    rustykrab_core::work::Edge {
        item: item.into(),
        depends_on: depends_on.into(),
        kind,
    }
}

/// A rung event as the controller writes it: `rung: outcome` becomes the
/// `RungEvent` JSON the controller stores; anything that does not name a
/// rung is kept as raw text, which no reader takes for a rung.
fn rung(item: &str, reason: &str) -> WorkEvent {
    let (name, outcome) = reason.split_once(':').unwrap_or((reason, ""));
    let encoded = serde_json::from_value::<rustykrab_core::work::Rung>(json!(name.trim()))
        .map(|rung| {
            rustykrab_control::ladder::encode_rung_event(&rustykrab_core::work::RungEvent {
                rung,
                at: Utc::now(),
                error: None,
                outcome: outcome.trim().to_string(),
            })
        })
        .unwrap_or_else(|_| reason.to_string());
    WorkEvent {
        item: item.into(),
        at: Utc::now(),
        kind: EventKind::Rung,
        from: None,
        to: None,
        actor: "controller".into(),
        reason: Some(encoded),
        upstream: None,
        origin: None,
        evidence_ref: None,
    }
}

/// Plan section 14.2's trip (p: f done, h failed, b and c held behind h),
/// one ready errand (r), a plan awaiting approval (t: x runs, y is held)
/// and one archived item (old) that b was discovered from.
async fn seed(store: &Store) {
    use BlockedReason::{NeedsConsent, UpstreamFailed};
    use WorkKind::{Personal, Research};
    let upstream_failed = Status::Blocked(UpstreamFailed);
    let mut b = under(
        cascaded(
            item(3, "b", Personal, "Book flight and hotel", upstream_failed),
            "h",
        ),
        "p",
    );
    b.inputs_from = vec!["f".into(), "h".into()];
    let mut y = planned(
        under(
            item(
                8,
                "y",
                Personal,
                "Switch the carrier",
                Status::Blocked(NeedsConsent),
            ),
            "t",
        ),
        true,
    );
    y.writable_resources = vec!["carrier account".into()];
    let mut old = item(9, "old", Personal, "Renew the passport", Status::Done);
    old.closed_at = Some(Utc::now() - TimeDelta::days(60));
    let items = vec![
        cascaded(
            item(0, "p", Personal, "Plan the Lisbon trip", upstream_failed),
            "h",
        ),
        under(
            item(1, "f", Research, "Find flight options", Status::Done),
            "p",
        ),
        under(
            item(
                2,
                "h",
                Research,
                "Find hotels near the venue",
                Status::Failed,
            ),
            "p",
        ),
        b,
        under(
            cascaded(
                item(
                    4,
                    "c",
                    Personal,
                    "Add the trip to the calendar",
                    upstream_failed,
                ),
                "h",
            ),
            "p",
        ),
        item(5, "r", Personal, "Renew the library card", Status::Ready),
        planned(
            item(
                6,
                "t",
                Personal,
                "Switch the phone plan",
                Status::Blocked(NeedsConsent),
            ),
            true,
        ),
        planned(
            under(
                item(7, "x", Research, "Compare three phone plans", Status::Ready),
                "t",
            ),
            false,
        ),
        y,
        old,
    ];
    let edges = vec![
        edge("b", EdgeKind::Blocks, "f"),
        edge("b", EdgeKind::Blocks, "h"),
        edge("b", EdgeKind::DiscoveredFrom, "old"),
        edge("c", EdgeKind::Blocks, "b"),
        edge("y", EdgeKind::Blocks, "x"),
    ];
    let plan = WorkPlanRow {
        id: "plan-1".into(),
        root: "t".into(),
        filed_by: None,
        rationale: "compare the plans, then switch".into(),
        approval_question: Some("q1".into()),
        policy: Some("approval.default".into()),
        created_at: Utc::now(),
    };
    store
        .work_insert_graph(&items, &edges, Some(&plan))
        .await
        .unwrap();
    for reason in [
        "retry: timeout",
        "retry: timeout again",
        "repair: narrower search",
        "switch_worker: peer",
    ] {
        store.work_event_append(&rung("h", reason)).await.unwrap();
    }
    store
        .work_event_append(&WorkEvent {
            kind: EventKind::Warning,
            reason: Some("kept for the record".into()),
            ..rung("old", "")
        })
        .await
        .unwrap();
    store
        .work_evidence_add(Evidence {
            item: "f".into(),
            kind: "url".into(),
            reference: "https://example.com/flights".into(),
            hash: None,
            verified_by: Some("controller".into()),
            at: Utc::now(),
        })
        .await
        .unwrap();
    assert_eq!(
        store
            .work_archive_compact(&["old".into()], Utc::now())
            .await
            .unwrap(),
        1
    );
}

struct Harness {
    base: String,
    client: reqwest::Client,
    control: Arc<StubControl>,
    store: Store,
}

async fn harness() -> Harness {
    harness_with(true).await
}

async fn harness_with(with_control: bool) -> Harness {
    let dir = std::env::temp_dir().join(format!("rk-work-routes-{}", Uuid::new_v4()));
    let store = Store::open(&dir, vec![9u8; 32]).expect("store opens");
    seed(&store).await;
    let control = Arc::new(StubControl {
        store: store.clone(),
        calls: Mutex::default(),
    });
    let mut state = AppState::new(
        store.clone(),
        vec![],
        Arc::new(UnusedProvider),
        TOKEN.into(),
    );
    if with_control {
        state = state.with_control(control.clone());
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = crate::router(state);
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let base = format!("http://{addr}");
    let mut headers = HeaderMap::new();
    headers.insert(ORIGIN, HeaderValue::from_str(&base).unwrap());
    let client = reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .unwrap();
    Harness {
        base,
        client,
        control,
        store,
    }
}

impl Harness {
    async fn send(&self, request: reqwest::RequestBuilder) -> (Http, Value) {
        let response = request.send().await.unwrap();
        let status = response.status();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    async fn get(&self, path: &str) -> (Http, Value) {
        self.send(
            self.client
                .get(format!("{}{path}", self.base))
                .bearer_auth(TOKEN),
        )
        .await
    }

    async fn post(&self, path: &str, body: Option<Value>) -> (Http, Value) {
        let mut request = self
            .client
            .post(format!("{}{path}", self.base))
            .bearer_auth(TOKEN);
        if let Some(body) = body {
            request = request.json(&body);
        }
        self.send(request).await
    }

    /// The ids of `GET {path}`'s rows, in order.
    async fn ids(&self, path: &str) -> Vec<String> {
        let (status, body) = self.get(path).await;
        assert_eq!(status, Http::OK, "{path}: {body}");
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["item"]["id"].as_str().unwrap().to_string())
            .collect()
    }
}

fn small_plan(items: usize) -> Value {
    let drafts: Vec<Value> = (0..items)
        .map(|n| {
            json!({
                "tmp": format!("t{n}"),
                "kind": "personal",
                "title": format!("step {n}"),
                "objective": "do it",
                "done_when": "it is done"
            })
        })
        .collect();
    json!({ "root": { "tmp": "t0" }, "items": drafts, "rationale": "one errand" })
}

// ── tests ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn every_route_answers() {
    let h = harness().await;
    for path in [
        "/api/work",
        "/api/work/ready",
        "/api/work/p",
        "/api/work/p/graph",
        "/api/work/h/events",
        "/api/work/f/evidence",
        "/api/work/t/plan",
        "/api/work/archive",
        "/api/work/archive/old",
    ] {
        let (status, body) = h.get(path).await;
        assert_eq!(status, Http::OK, "GET {path}: {body}");
    }
    for path in [
        "/api/work/t/approve",
        "/api/work/t/reject",
        "/api/work/c/cancel",
        "/api/work/tick",
    ] {
        let (status, body) = h.post(path, None).await;
        assert_eq!(status, Http::OK, "POST {path}: {body}");
    }
    let (status, body) = h.post("/api/work/plan", Some(small_plan(1))).await;
    assert_eq!(status, Http::OK, "{body}");

    let (_, tick) = h.post("/api/work/tick", None).await;
    let report: TickReport = serde_json::from_value(tick).unwrap();
    assert_eq!(report.made_ready, vec!["r".to_string()]);
}

#[tokio::test]
async fn list_filters_by_status_kind_parent_and_closed() {
    let h = harness().await;
    assert_eq!(
        h.ids("/api/work").await,
        ["p", "b", "c", "r", "t", "x", "y"]
    );
    assert_eq!(h.ids("/api/work/ready").await, ["r", "x"]);
    assert_eq!(h.ids("/api/work?status=ready").await, ["r", "x"]);
    assert_eq!(
        h.ids("/api/work?status=blocked").await,
        ["p", "b", "c", "t", "y"]
    );
    assert_eq!(
        h.ids("/api/work?status=blocked(needs_consent)").await,
        ["t", "y"]
    );
    assert_eq!(
        h.ids("/api/work?status=blocked:upstream_failed").await,
        ["p", "b", "c"]
    );
    assert_eq!(h.ids("/api/work?status=done").await, ["f"]);
    assert!(h.ids("/api/work?status=cancelled").await.is_empty());
    assert_eq!(h.ids("/api/work?kind=research").await, ["x"]);
    assert_eq!(
        h.ids("/api/work?kind=research&include_closed=true").await,
        ["f", "h", "x"]
    );
    assert_eq!(h.ids("/api/work?parent=p").await, ["b", "c"]);
    assert_eq!(
        h.ids("/api/work?parent=p&include_closed=true").await,
        ["f", "h", "b", "c"]
    );

    for bad in [
        "/api/work?status=bogus",
        "/api/work?status=ready(now)",
        "/api/work?status=blocked(bogus)",
        "/api/work?kind=bogus",
        "/api/work?include_closed=maybe",
    ] {
        let (status, body) = h.get(bad).await;
        assert_eq!(status, Http::BAD_REQUEST, "{bad}");
        assert_eq!(body["error"], "invalid_request", "{bad}");
    }

    // A parent carries its roll-up; a leaf does not. A cascade status
    // names its origin.
    let (_, body) = h.get("/api/work").await;
    let rows = body["items"].as_array().unwrap();
    let p = &rows[0];
    assert_eq!(p["rollup"]["children_done"], 1);
    assert_eq!(p["rollup"]["children_total"], 4);
    assert_eq!(
        p["rollup"]["status"],
        json!({ "status": "blocked", "reason": "upstream_failed" })
    );
    let b = &rows[1];
    assert!(b["rollup"].is_null());
    assert_eq!(b["item"]["status_origin"], "h");
    let parsed: super::WorkList = serde_json::from_value(body).unwrap();
    assert_eq!(parsed.items.len(), 7);
}

#[tokio::test]
async fn a_rejected_plan_returns_422_with_every_failed_check() {
    let h = harness().await;
    let (status, body) = h.post("/api/work/plan", Some(small_plan(4))).await;
    assert_eq!(status, Http::UNPROCESSABLE_ENTITY);
    assert_eq!(body["outcome"], "rejected");
    let reasons: Vec<&str> = body["failed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["reason"].as_str().unwrap())
        .collect();
    assert_eq!(reasons, ["too_many_items", "cycle"]);
    assert_eq!(
        body["failed"][1]["offending"],
        json!([{ "tmp": "a" }, { "tmp": "b" }])
    );

    let (status, body) = h.post("/api/work/plan", Some(small_plan(1))).await;
    assert_eq!(status, Http::OK);
    assert_eq!(body["outcome"], "accepted");
    assert_eq!(body["ids"], json!({ "t0": "new-t0" }));

    // A body that is not a work_plan never reaches the controller.
    let (status, body) = h
        .post("/api/work/plan", Some(json!({ "items": "nope" })))
        .await;
    assert_eq!(status, Http::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_request");

    assert_eq!(
        h.control.calls(),
        [
            Call::FilePlan {
                actor: "user:master".into(),
                source: FilingSource::Planner,
                items: 4,
                origin: None,
            },
            Call::FilePlan {
                actor: "user:master".into(),
                source: FilingSource::Planner,
                items: 1,
                origin: None,
            },
        ]
    );
}

#[tokio::test]
async fn approve_reject_and_cancel_pass_the_actor_through() {
    let h = harness().await;

    let (status, body) = h.post("/api/work/t/approve", None).await;
    assert_eq!(status, Http::OK);
    assert_eq!(body, json!({ "root": "t", "released": ["y"] }));

    let (status, body) = h
        .post(
            "/api/work/t/reject",
            Some(json!({ "reason": "too pricey" })),
        )
        .await;
    assert_eq!(status, Http::OK);
    assert_eq!(body["cancelled"], json!(["y"]));
    assert_eq!(body["already_finished"], json!([]));

    // Cancel reports what it cancelled and what had already finished.
    let (status, body) = h.post("/api/work/p/cancel", None).await;
    assert_eq!(status, Http::OK);
    let reply: super::CancelReply = serde_json::from_value(body).unwrap();
    assert_eq!(reply.cancelled, ["p", "b", "c"]);
    let finished: Vec<(&str, Status)> = reply
        .already_finished
        .iter()
        .map(|f| (f.id.as_str(), f.status))
        .collect();
    assert_eq!(finished, [("f", Status::Done), ("h", Status::Failed)]);

    // A paired device is its own actor.
    let devices = h.store.devices();
    let code = devices.mint_pairing_code().await.unwrap();
    let (_, device_token) = devices.redeem_pairing_code(&code, "phone").await.unwrap();
    let response = h
        .client
        .post(format!("{}/api/work/t/cancel", h.base))
        .bearer_auth(&device_token)
        .json(&json!({ "reason": "  changed my mind  " }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), Http::OK);

    assert_eq!(
        h.control.calls(),
        [
            Call::Approve {
                root: "t".into(),
                actor: "user:master".into(),
            },
            Call::Reject {
                root: "t".into(),
                reason: Some("too pricey".into()),
                actor: "user:master".into(),
            },
            Call::Cancel {
                item: "p".into(),
                reason: None,
                actor: "user:master".into(),
            },
            Call::Cancel {
                item: "t".into(),
                reason: Some("changed my mind".into()),
                actor: "user:phone".into(),
            },
        ]
    );

    // Unknown and archived ids, and a body that is not JSON.
    assert_eq!(
        h.post("/api/work/nope/cancel", None).await.0,
        Http::NOT_FOUND
    );
    let (status, body) = h.post("/api/work/old/cancel", None).await;
    assert_eq!(status, Http::GONE);
    assert_eq!(body["error"], "archived");
    let response = h
        .client
        .post(format!("{}/api/work/t/reject", h.base))
        .bearer_auth(TOKEN)
        .body("not json")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), Http::BAD_REQUEST);
}

#[tokio::test]
async fn the_archive_is_searchable_and_stays_out_of_live_reads() {
    let h = harness().await;
    let (status, body) = h.get("/api/work/archive?q=passport").await;
    assert_eq!(status, Http::OK);
    let found = body["archived"].as_array().unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0]["id"], "old");
    assert!(found[0]["summary"]
        .as_str()
        .unwrap()
        .contains("Renew the passport"));

    for (query, expected) in [
        ("q=nothing%20like%20this", 0),
        ("q=passport&kind=research", 0),
        ("q=passport&kind=personal", 1),
        ("kind=personal", 1),
        ("since=2999-01-01", 0),
        ("since=2000-01-01T00:00:00Z", 1),
    ] {
        let (status, body) = h.get(&format!("/api/work/archive?{query}")).await;
        assert_eq!(status, Http::OK, "{query}");
        assert_eq!(
            body["archived"].as_array().unwrap().len(),
            expected,
            "{query}"
        );
    }
    assert_eq!(
        h.get("/api/work/archive?since=yesterday").await.0,
        Http::BAD_REQUEST
    );

    let (status, body) = h.get("/api/work/archive/old").await;
    assert_eq!(status, Http::OK);
    assert_eq!(body["title"], "Renew the passport");
    assert_eq!(h.get("/api/work/archive/nope").await.0, Http::NOT_FOUND);

    // Live reads: gone from the item routes, but events outlive it, and a
    // live item's edge onto it resolves to its line.
    let (status, body) = h.get("/api/work/old").await;
    assert_eq!(status, Http::GONE);
    assert_eq!(body["error"], "archived");
    assert!(!h
        .ids("/api/work?include_closed=true")
        .await
        .contains(&"old".to_string()));
    let (status, body) = h.get("/api/work/old/events").await;
    assert_eq!(status, Http::OK);
    assert_eq!(body["events"].as_array().unwrap().len(), 1);
    let (_, body) = h.get("/api/work/b").await;
    let detail: super::ItemDetail = serde_json::from_value(body).unwrap();
    let onto_old = detail
        .edges
        .iter()
        .find(|e| e.edge.depends_on == "old")
        .unwrap();
    assert_eq!(onto_old.edge.kind, EdgeKind::DiscoveredFrom);
    assert!(onto_old.archived.as_deref().unwrap().contains("passport"));
    assert!(detail
        .edges
        .iter()
        .filter(|e| e.edge.depends_on != "old")
        .all(|e| e.archived.is_none()));
    assert_eq!(detail.dependents.len(), 1);
}

#[tokio::test]
async fn the_archive_takes_search_as_well_as_q() {
    let h = harness().await;
    let (status, body) = h.get("/api/work/archive?search=passport").await;
    assert_eq!(status, Http::OK);
    assert_eq!(body["archived"][0]["id"], "old");
    let (_, body) = h
        .get("/api/work/archive?search=nothing%20like%20this")
        .await;
    assert!(body["archived"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn one_draft_files_as_work_file_with_its_origin_conversation() {
    let h = harness().await;
    let draft = json!({
        "kind": "personal",
        "title": "Renew the parking permit",
        "objective": "renew it online",
        "done_when": "the renewal confirmation is attached",
        "origin_conversation_id": "conv-7",
    });
    let (status, body) = h.post("/api/work", Some(draft)).await;
    assert_eq!(status, Http::CREATED, "{body}");
    assert_eq!(body["outcome"], "accepted");

    let (status, body) = h.post("/api/work", Some(json!({ "title": 3 }))).await;
    assert_eq!(status, Http::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_request");

    assert_eq!(
        h.control.calls(),
        [Call::FilePlan {
            actor: "user:master".into(),
            source: FilingSource::WorkFile,
            items: 1,
            origin: Some("conv-7".into()),
        }]
    );
}

#[tokio::test]
async fn a_stack_manifest_imports_as_the_delivery_import() {
    let h = harness().await;
    let manifest = json!({
        "slice": { "id": "s1", "title": "Work items over REST", "objective": "o" },
        "layers": [
            { "id": "l1", "title": "Persist", "acceptance": "survives a restart",
              "parent_layer": null,
              "work_items": [
                  { "id": "w1", "title": "Table", "objective": "o", "done_when": "d",
                    "delivery_dependencies": [] },
                  { "id": "w2", "title": "Store API", "objective": "o", "done_when": "d",
                    "delivery_dependencies": ["w1"] } ] },
            { "id": "l2", "title": "List", "acceptance": "lists open items",
              "parent_layer": "l1",
              "work_items": [
                  { "id": "w3", "title": "Route", "objective": "o", "done_when": "d",
                    "delivery_dependencies": [] } ] },
        ],
    });
    // The stub rejects any plan of more than three items, so the six-item
    // slice comes back whole as 422 with every failed check.
    let (status, body) = h
        .post("/api/work/import", Some(json!({ "manifest": manifest })))
        .await;
    assert_eq!(status, Http::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["outcome"], "rejected");
    let (status, _) = h
        .post(
            "/api/work/import",
            Some(json!({ "manifest": { "layers": [] } })),
        )
        .await;
    assert_eq!(status, Http::BAD_REQUEST);
    assert_eq!(
        h.control.calls(),
        [Call::FilePlan {
            actor: "user:master".into(),
            source: FilingSource::DeliveryImport,
            items: 6,
            origin: None,
        }]
    );
}

/// Frames of `GET /api/work/events` until `want` frames arrived or two
/// seconds passed, as `(event, id, data)`.
async fn read_frames(mut response: reqwest::Response, want: usize) -> Vec<(String, String, Value)> {
    let mut buffer = String::new();
    let mut frames = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while frames.len() < want {
        let Ok(Ok(Some(chunk))) = tokio::time::timeout_at(deadline, response.chunk()).await else {
            break;
        };
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(end) = buffer.find("\n\n") {
            let frame: String = buffer.drain(..end + 2).collect();
            let (mut kind, mut id, mut data) = (String::new(), String::new(), String::new());
            for line in frame.lines() {
                if let Some(v) = line.strip_prefix("event:") {
                    kind = v.trim().to_string();
                } else if let Some(v) = line.strip_prefix("id:") {
                    id = v.trim().to_string();
                } else if let Some(v) = line.strip_prefix("data:") {
                    data.push_str(v.trim());
                }
            }
            if !data.is_empty() {
                frames.push((kind, id, serde_json::from_str(&data).unwrap()));
            }
        }
    }
    frames
}

#[tokio::test]
async fn the_progress_stream_sends_each_new_event_once() {
    let h = harness().await;
    let open = |last: Option<&str>| {
        let mut request = h
            .client
            .get(format!("{}/api/work/events", h.base))
            .bearer_auth(TOKEN);
        if let Some(id) = last {
            request = request.header("last-event-id", id);
        }
        request.send()
    };
    let response = open(None).await.unwrap();
    assert_eq!(response.status(), Http::OK);
    assert!(response.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/event-stream"));
    // Seeded history is not replayed; what happens next is.
    h.store
        .work_event_append(&rung("h", "repair: after the stream opened"))
        .await
        .unwrap();
    let frames = read_frames(response, 1).await;
    assert_eq!(frames.len(), 1, "{frames:?}");
    let (kind, id, data) = &frames[0];
    assert_eq!(kind, "rung");
    assert_eq!(data["item"], "h");
    assert_eq!(data["kind"], "rung");
    assert!(data["actor"].is_string() && data.get("origin").is_some());

    // A client resuming after that id sees only what came later.
    h.store
        .work_event_append(&rung("h", "retry: later still"))
        .await
        .unwrap();
    let frames = read_frames(open(Some(id)).await.unwrap(), 2).await;
    assert_eq!(frames.len(), 1, "{frames:?}");
    assert!(frames[0].2["reason"]
        .as_str()
        .unwrap()
        .contains("later still"));
}

#[tokio::test]
async fn unauthenticated_requests_are_refused_like_the_other_routes() {
    let h = harness().await;
    for (method, path) in [
        ("GET", "/api/work"),
        ("GET", "/api/work/p/graph"),
        ("POST", "/api/work/plan"),
        ("POST", "/api/work/t/approve"),
        ("GET", "/api/tasks"),
    ] {
        let url = format!("{}{path}", h.base);
        let bare = match method {
            "GET" => h.client.get(&url),
            _ => h.client.post(&url).json(&small_plan(1)),
        };
        assert_eq!(
            bare.send().await.unwrap().status(),
            Http::UNAUTHORIZED,
            "{method} {path} without a token"
        );
        let wrong = match method {
            "GET" => h.client.get(&url),
            _ => h.client.post(&url).json(&small_plan(1)),
        };
        assert_eq!(
            wrong.bearer_auth("wrong").send().await.unwrap().status(),
            Http::UNAUTHORIZED,
            "{method} {path} with a wrong token"
        );
    }
    // The origin check stands in front of the work routes too.
    let no_origin = reqwest::Client::new()
        .get(format!("{}/api/work", h.base))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(no_origin.status(), Http::FORBIDDEN);
    assert!(h.control.calls().is_empty());
}

#[tokio::test]
async fn the_graph_carries_rungs_and_cascade_origins() {
    let h = harness().await;
    let (status, body) = h.get("/api/work/p/graph").await;
    assert_eq!(status, Http::OK);
    assert_eq!(body["root"], "p");
    assert_eq!(
        body["rungs"],
        json!({ "h": ["retry", "repair", "switch_worker"] })
    );
    let reply: GraphReply = serde_json::from_value(body).unwrap();
    let ids: Vec<&str> = reply
        .graph
        .nodes
        .iter()
        .map(|n| n.item.id.as_str())
        .collect();
    assert_eq!(ids, ["p", "f", "h", "b", "c"]);
    let b = &reply.graph.nodes[3];
    assert_eq!(
        b.item.status,
        Status::Blocked(BlockedReason::UpstreamFailed)
    );
    assert_eq!(b.item.status_origin.as_deref(), Some("h"));
    assert_eq!(b.edges.len(), 3);

    assert_eq!(h.get("/api/work/nope/graph").await.0, Http::NOT_FOUND);
    assert_eq!(h.get("/api/work/old/graph").await.0, Http::GONE);
}

#[tokio::test]
async fn the_plan_preview_names_the_pending_plan_and_its_holds() {
    let h = harness().await;
    let (status, body) = h.get("/api/work/t/plan").await;
    assert_eq!(status, Http::OK, "{body}");
    let preview: super::PlanPreview = serde_json::from_value(body).unwrap();
    assert_eq!(preview.plan.id, "plan-1");
    assert_eq!(preview.plan.rationale, "compare the plans, then switch");
    assert_eq!(preview.plan.policy.as_deref(), Some("approval.default"));
    assert_eq!(preview.held, ["t", "y"]);
    assert_eq!(preview.graph.nodes.len(), 3);

    let (status, body) = h.get("/api/work/r/plan").await;
    assert_eq!(status, Http::NOT_FOUND);
    assert_eq!(body["error"], "no_plan");
}

#[tokio::test]
async fn item_detail_carries_its_ladder_evidence_and_events() {
    let h = harness().await;
    let (_, body) = h.get("/api/work/h").await;
    let detail: super::ItemDetail = serde_json::from_value(body.clone()).unwrap();
    assert_eq!(detail.events.len(), 4);
    assert!(detail.evidence.is_empty());
    assert!(detail.lease.is_none());
    let climbed: Vec<&str> = detail.ladder.iter().map(|r| r.rung.as_str()).collect();
    assert_eq!(climbed, ["retry", "retry", "repair", "switch_worker"]);
    assert_eq!(body["events"][0]["kind"], "rung");
    let (_, body) = h.get("/api/work/f").await;
    assert_eq!(body["evidence"].as_array().unwrap().len(), 1);
    let (_, body) = h.get("/api/work/p").await;
    assert_eq!(body["rollup"]["children_total"], 4);
    let (status, body) = h.get("/api/work/f/evidence").await;
    assert_eq!(status, Http::OK);
    assert_eq!(
        body["evidence"][0]["reference"],
        "https://example.com/flights"
    );
    assert_eq!(h.get("/api/work/nope").await.0, Http::NOT_FOUND);
    assert_eq!(h.get("/api/work/nope/events").await.0, Http::NOT_FOUND);
    assert_eq!(h.get("/api/work/nope/evidence").await.0, Http::NOT_FOUND);
}

#[tokio::test]
async fn without_a_controller_reads_answer_and_commands_do_not() {
    let h = harness_with(false).await;
    let (status, body) = h.get("/api/work").await;
    assert_eq!(status, Http::OK);
    let p = &body["items"][0];
    assert_eq!(p["item"]["id"], "p");
    assert_eq!(p["rollup"]["children_done"], 1);
    assert_eq!(p["rollup"]["children_total"], 4);
    assert_eq!(h.get("/api/work/p").await.0, Http::OK);
    assert_eq!(h.get("/api/work/archive").await.0, Http::OK);

    for (status, body) in [
        h.post("/api/work/tick", None).await,
        h.post("/api/work/plan", Some(small_plan(1))).await,
        h.post("/api/work/t/approve", None).await,
        h.get("/api/work/p/graph").await,
    ] {
        assert_eq!(status, Http::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"], "control_unavailable");
    }
}

#[test]
fn status_filters_are_strict() {
    assert_eq!(
        parse_status_filter("ready"),
        Ok(StatusFilter::Exact(Status::Ready))
    );
    assert_eq!(
        parse_status_filter("blocked"),
        Ok(StatusFilter::Name("blocked"))
    );
    assert_eq!(
        parse_status_filter("blocked(needs_consent)"),
        Ok(StatusFilter::Exact(Status::Blocked(
            BlockedReason::NeedsConsent
        )))
    );
    assert_eq!(
        parse_status_filter("cancelled:superseded"),
        Ok(StatusFilter::Exact(Status::Cancelled(
            rustykrab_core::work::CancelReason::Superseded
        )))
    );
    assert!(parse_status_filter("done(now)").is_err());
    assert!(parse_status_filter("blocked(nope)").is_err());
    assert!(parse_status_filter("finished").is_err());
}

#[test]
fn rungs_are_read_from_rung_events_in_first_climbed_order() {
    let events = vec![
        rung("h", "repair: narrower"),
        rung("h", "retry: timeout"),
        rung("h", "repair: again"),
        WorkEvent {
            kind: EventKind::Warning,
            ..rung("h", "surface: not a rung event")
        },
        rung("h", "not_a_rung: ignored"),
    ];
    assert_eq!(
        super::rungs_climbed(&events),
        [
            rustykrab_core::work::Rung::Repair,
            rustykrab_core::work::Rung::Retry
        ]
    );
}
