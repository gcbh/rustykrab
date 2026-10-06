use chrono::{DateTime, TimeDelta, Utc};

use rustykrab_core::work::{
    ArtifactRef, BlockedReason, Budget, CancelReason, Edge, EdgeKind, EventKind, Evidence,
    InputRef, Precondition, RungBudgets, Status, Trigger, WorkEvent, WorkItem, WorkKind,
    WorkerKind,
};

use super::*;
use crate::Store;

fn at(secs: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(1_790_000_000 + secs, 123_456_789).unwrap()
}

fn item(id: &str, status: Status) -> WorkItem {
    WorkItem {
        id: id.into(),
        kind: WorkKind::Personal,
        title: format!("item {id}"),
        objective: "objective".into(),
        done_when: "done when".into(),
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
        created_at: at(0),
        updated_at: at(0),
        closed_at: status.is_closed().then(|| at(1)),
    }
}

fn edge(item: &str, kind: EdgeKind, depends_on: &str) -> Edge {
    Edge {
        item: item.into(),
        depends_on: depends_on.into(),
        kind,
    }
}

fn to(id: &str, status: Status) -> TransitionSpec {
    TransitionSpec::new(id, status, "controller")
}

async fn seeded(items: Vec<WorkItem>, edges: Vec<Edge>) -> Store {
    let store = Store::open_in_memory();
    store.work_insert_graph(&items, &edges, None).await.unwrap();
    store
}

fn sql(store: &Store, statement: &str) {
    store.conn.lock().unwrap().execute(statement, []).unwrap();
}

async fn status_of(store: &Store, id: &str) -> Status {
    store.work_get(id).await.unwrap().unwrap().status
}

#[tokio::test]
async fn every_field_round_trips() {
    let full = WorkItem {
        id: "w1".into(),
        kind: WorkKind::Code,
        title: "Fix the flaky scenario".into(),
        objective: "make scenario 14 pass ten times in a row".into(),
        done_when: "ten green runs".into(),
        constraints: vec!["no new dependencies".into(), "keep the timeout".into()],
        decisions_made: vec!["retry is not the fix".into()],
        artifact_refs: vec![ArtifactRef {
            kind: "commit".into(),
            value: "1d1608b".into(),
        }],
        required_tools: vec!["shell".into()],
        required_mcp_servers: vec!["github".into()],
        worker_kind: WorkerKind::ClaudeCode,
        writable_resources: vec!["worktree:rk".into()],
        parent: Some("p1".into()),
        inputs_from: vec!["a".into(), "b".into()],
        origin_conversation_id: Some("conv-9".into()),
        trigger: Trigger::At(at(3_600)),
        preconditions: vec![Precondition {
            name: "on_power".into(),
            args: serde_json::json!({ "min_battery": 40 }),
        }],
        expires_at: Some(at(86_400)),
        budget: Budget {
            iterations: 7,
            tokens: 12_345,
            wall_seconds: 99,
            repairs: 1,
            rungs: RungBudgets {
                retries: 3,
                ..RungBudgets::default()
            },
        },
        priority: -3,
        status: Status::Blocked(BlockedReason::NeedsDecision),
        status_origin: Some("o1".into()),
        plan_id: Some("plan-1".into()),
        held_by: Some("q-1".into()),
        created_at: at(10),
        updated_at: at(20),
        closed_at: None,
    };
    let store = seeded(vec![full.clone()], vec![]).await;
    assert_eq!(store.work_get("w1").await.unwrap(), Some(full));

    // The scalar columns the controller filters on are written alongside
    // the JSON, not left for a JSON scan.
    let (trigger_at, reason): (Option<String>, Option<String>) = store
        .conn
        .lock()
        .unwrap()
        .query_row(
            "SELECT trigger_at, status_reason FROM work_items WHERE id = 'w1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(trigger_at, Some(rows::ts(&at(3_600))));
    assert_eq!(reason.as_deref(), Some("needs_decision"));
}

#[tokio::test]
async fn every_status_round_trips() {
    let mut all = vec![
        Status::Queued,
        Status::Ready,
        Status::Leased,
        Status::Running,
        Status::Verifying,
        Status::Done,
        Status::Failed,
        Status::Expired,
    ];
    all.extend(BlockedReason::ALL.iter().map(|r| Status::Blocked(*r)));
    all.extend(
        [
            CancelReason::Requested,
            CancelReason::Cascade,
            CancelReason::Superseded,
        ]
        .map(Status::Cancelled),
    );
    let items: Vec<WorkItem> = all
        .iter()
        .enumerate()
        .map(|(i, s)| item(&format!("s{i:02}"), *s))
        .collect();
    let store = seeded(items.clone(), vec![]).await;
    for expected in &items {
        let got = store.work_get(&expected.id).await.unwrap().unwrap();
        assert_eq!(got.status, expected.status, "{}", expected.status);
        assert_eq!(&got, expected);
    }
}

#[tokio::test]
async fn closed_is_final() {
    let store = seeded(vec![item("a", Status::Running)], vec![]).await;

    let event = store
        .work_transition(
            "a",
            None,
            Status::Done,
            "controller",
            None,
            None,
            None,
            Some("ev-1"),
        )
        .await
        .unwrap();
    assert_eq!(event.from, Some(Status::Running));
    assert_eq!(event.to, Some(Status::Done));
    assert_eq!(event.kind, EventKind::Transition);
    let done = store.work_get("a").await.unwrap().unwrap();
    assert_eq!(
        done.closed_at,
        Some(event.at),
        "entering a closed status stamps closed_at"
    );

    let refused = store
        .work_transition("a", None, Status::Ready, "user", None, None, None, None)
        .await;
    assert_eq!(
        refused,
        Err(WorkStoreError::Closed {
            item: "a".into(),
            status: Status::Done
        })
    );
    assert_eq!(status_of(&store, "a").await, Status::Done);
    assert_eq!(
        store.work_events("a").await.unwrap().len(),
        1,
        "a refused transition leaves no event"
    );
}

#[tokio::test]
async fn a_stale_expected_status_is_refused() {
    let store = seeded(vec![item("a", Status::Queued)], vec![]).await;
    let refused = store
        .work_transition_many(&[TransitionSpec {
            expected_from: Some(Status::Ready),
            ..to("a", Status::Leased)
        }])
        .await;
    assert_eq!(
        refused,
        Err(WorkStoreError::StatusMismatch {
            item: "a".into(),
            expected: Status::Ready,
            actual: Status::Queued
        })
    );
    assert_eq!(status_of(&store, "a").await, Status::Queued);
}

#[tokio::test]
async fn transition_many_is_all_or_nothing() {
    let store = seeded(
        vec![item("a", Status::Queued), item("b", Status::Done)],
        vec![],
    )
    .await;

    let refused = store
        .work_transition_many(&[to("a", Status::Ready), to("b", Status::Ready)])
        .await;
    assert!(
        matches!(refused, Err(WorkStoreError::Closed { .. })),
        "{refused:?}"
    );
    assert_eq!(
        status_of(&store, "a").await,
        Status::Queued,
        "the good transition before the bad one rolled back with it"
    );
    assert!(store.work_events("a").await.unwrap().is_empty());
}

#[tokio::test]
async fn a_cascade_records_its_origin_on_the_row_and_the_event() {
    let store = seeded(
        vec![item("up", Status::Failed), item("down", Status::Queued)],
        vec![edge("down", EdgeKind::Blocks, "up")],
    )
    .await;
    let events = store
        .work_transition_many(&[TransitionSpec {
            upstream: Some("up".into()),
            origin: Some("up".into()),
            ..to("down", Status::Blocked(BlockedReason::UpstreamFailed))
        }])
        .await
        .unwrap();
    assert_eq!(events[0].kind, EventKind::Cascade, "picked from the status");
    assert_eq!(events[0].origin.as_deref(), Some("up"));
    let down = store.work_get("down").await.unwrap().unwrap();
    assert_eq!(down.status_origin.as_deref(), Some("up"));

    // A later transition that names no origin clears it: the column
    // describes the current status, not the item's history.
    store
        .work_transition_many(&[to("down", Status::Queued)])
        .await
        .unwrap();
    let down = store.work_get("down").await.unwrap().unwrap();
    assert_eq!(down.status_origin, None);
}

#[tokio::test]
async fn a_lease_is_exclusive_and_moves_the_item_to_leased() {
    let store = seeded(
        vec![item("r", Status::Ready), item("q", Status::Queued)],
        vec![],
    )
    .await;
    let inputs = vec![InputRef {
        item: "up".into(),
        title: "research".into(),
        status: Status::Done,
        edge: Some(EdgeKind::Blocks),
        evidence: vec![],
        artifacts: vec![],
        summary: "found three".into(),
        error: None,
    }];

    let lease = store
        .work_lease_acquire("r", "pinch", 600, inputs.clone())
        .await
        .unwrap();
    assert_eq!(lease.inputs, inputs);
    assert_eq!(
        store.work_lease_get("r").await.unwrap(),
        Some(lease.clone())
    );
    assert_eq!(status_of(&store, "r").await, Status::Leased);
    let events = store.work_events("r").await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, EventKind::Lease);
    assert_eq!(events[0].actor, "worker:pinch");
    assert_eq!(
        (events[0].from, events[0].to),
        (Some(Status::Ready), Some(Status::Leased))
    );

    let again = store.work_lease_acquire("r", "other", 600, vec![]).await;
    assert_eq!(
        again,
        Err(WorkStoreError::LeaseHeld {
            item: "r".into(),
            worker: "pinch".into()
        })
    );
    assert!(matches!(
        store.work_lease_acquire("q", "pinch", 600, vec![]).await,
        Err(WorkStoreError::StatusMismatch { .. })
    ));
}

#[tokio::test]
async fn expired_leases_are_found_by_heartbeat_and_ttl() {
    let store = seeded(
        vec![item("short", Status::Ready), item("long", Status::Ready)],
        vec![],
    )
    .await;
    store
        .work_lease_acquire("short", "pinch", 60, vec![])
        .await
        .unwrap();
    store
        .work_lease_acquire("long", "pinch", 3_600, vec![])
        .await
        .unwrap();

    let now = Utc::now();
    assert!(store.work_leases_expired(now).await.unwrap().is_empty());
    let later = now + TimeDelta::seconds(120);
    let expired = store.work_leases_expired(later).await.unwrap();
    assert_eq!(
        expired.iter().map(|l| l.item.as_str()).collect::<Vec<_>>(),
        ["short"]
    );

    let beat = store.work_lease_heartbeat("long").await.unwrap();
    assert!(beat.heartbeat_at >= beat.since);
    let released = store.work_lease_release("short").await.unwrap();
    assert_eq!(released.map(|l| l.worker).as_deref(), Some("pinch"));
    assert_eq!(store.work_lease_release("short").await.unwrap(), None);
    assert!(matches!(
        store.work_lease_heartbeat("short").await,
        Err(WorkStoreError::NotFound(_))
    ));
}

#[tokio::test]
async fn leaving_active_status_drops_the_lease() {
    let store = seeded(vec![item("a", Status::Ready)], vec![]).await;
    store
        .work_lease_acquire("a", "pinch", 600, vec![])
        .await
        .unwrap();
    store
        .work_transition_many(&[to("a", Status::Running), to("a", Status::Verifying)])
        .await
        .unwrap();
    assert!(
        store.work_lease_get("a").await.unwrap().is_some(),
        "active statuses keep it"
    );
    store
        .work_transition_many(&[to("a", Status::Failed)])
        .await
        .unwrap();
    assert_eq!(store.work_lease_get("a").await.unwrap(), None);
}

#[tokio::test]
async fn repoint_rewrites_in_place_and_logs_the_old_value() {
    let mut downstream = item("e", Status::Queued);
    downstream.inputs_from = vec!["s".into(), "x".into()];
    let store = seeded(
        vec![
            item("s", Status::Failed),
            item("f", Status::Ready),
            item("x", Status::Done),
            downstream,
        ],
        vec![
            edge("e", EdgeKind::Blocks, "s"),
            edge("e", EdgeKind::Blocks, "x"),
        ],
    )
    .await;

    let event = store
        .work_repoint_edge("e", EdgeKind::Blocks, "s", "f", Some("s"), "controller")
        .await
        .unwrap();
    assert_eq!(
        store.work_edges_of("e").await.unwrap(),
        vec![
            edge("e", EdgeKind::Blocks, "f"),
            edge("e", EdgeKind::Blocks, "x")
        ]
    );
    assert!(store.work_dependents_of("s").await.unwrap().is_empty());
    let e = store.work_get("e").await.unwrap().unwrap();
    assert_eq!(e.inputs_from, ["f", "x"], "the input follows its edge");

    assert_eq!(event.kind, EventKind::Repoint);
    assert_eq!(event.upstream.as_deref(), Some("s"), "the old value");
    assert_eq!(event.origin.as_deref(), Some("s"));
    assert!(event.reason.as_deref().unwrap().contains("to f"));
    assert_eq!(store.work_events("e").await.unwrap(), vec![event]);

    // An input with no edge of its own moves on its own.
    store
        .work_repoint_input("e", "x", "f", None, "controller")
        .await
        .unwrap();
    let e = store.work_get("e").await.unwrap().unwrap();
    assert_eq!(e.inputs_from, ["f"], "and does not duplicate");

    assert!(matches!(
        store
            .work_repoint_edge("e", EdgeKind::WaitsFor, "s", "f", None, "controller")
            .await,
        Err(WorkStoreError::NotFound(_))
    ));
}

#[tokio::test]
async fn removing_an_items_edges_keeps_its_history() {
    let store = seeded(
        vec![item("t", Status::Cancelled(CancelReason::Superseded))],
        vec![
            edge("t", EdgeKind::Blocks, "a"),
            edge("t", EdgeKind::WaitsFor, "b"),
            edge("t", EdgeKind::DiscoveredFrom, "d"),
            edge("t", EdgeKind::Supersedes, "o"),
        ],
    )
    .await;
    assert_eq!(store.work_remove_edges_of("t").await.unwrap(), 2);
    assert_eq!(
        store.work_edges_of("t").await.unwrap(),
        vec![
            edge("t", EdgeKind::DiscoveredFrom, "d"),
            edge("t", EdgeKind::Supersedes, "o"),
        ]
    );
}

#[tokio::test]
async fn compaction_keeps_events_and_evidence_and_drops_deps() {
    let mut child = item("c", Status::Ready);
    child.kind = WorkKind::Research;
    child.title = "Find hotels near the venue".into();
    child.parent = Some("p".into());
    let store = seeded(
        vec![
            child,
            item("a", Status::Done),
            item("d", Status::Queued),
            item("open", Status::Queued),
            item("closed", Status::Done),
        ],
        vec![
            edge("c", EdgeKind::Blocks, "a"),
            edge("d", EdgeKind::WaitsFor, "c"),
        ],
    )
    .await;
    store
        .work_lease_acquire("c", "pinch", 600, vec![])
        .await
        .unwrap();
    store
        .work_transition_many(&[to("c", Status::Running), to("c", Status::Done)])
        .await
        .unwrap();
    store
        .work_evidence_add(Evidence {
            item: "c".into(),
            kind: "url".into(),
            reference: "https://example.com/hotels".into(),
            hash: None,
            verified_by: Some("url_check".into()),
            at: at(5),
        })
        .await
        .unwrap();

    // One open id refuses the whole call.
    assert!(matches!(
        store
            .work_archive_compact(&["closed".into(), "open".into()], at(100))
            .await,
        Err(WorkStoreError::NotClosed { .. })
    ));
    assert!(store.work_get("closed").await.unwrap().is_some());

    assert_eq!(
        store
            .work_archive_compact(&["c".into()], at(100))
            .await
            .unwrap(),
        1
    );
    assert_eq!(store.work_get("c").await.unwrap(), None);
    assert!(store.work_edges_of("c").await.unwrap().is_empty());
    assert_eq!(
        store.work_dependents_of("c").await.unwrap(),
        vec![edge("d", EdgeKind::WaitsFor, "c")],
        "edges held by live items are theirs, not the archived item's"
    );
    assert_eq!(store.work_events("c").await.unwrap().len(), 3);
    assert_eq!(store.work_evidence_list("c").await.unwrap().len(), 1);

    let line = store.work_archive_get("c").await.unwrap().unwrap();
    assert_eq!(line.kind, WorkKind::Research);
    assert_eq!(line.status, Status::Done);
    assert_eq!(line.parent.as_deref(), Some("p"));
    assert_eq!(line.worker.as_deref(), Some("pinch"));
    assert_eq!(line.edges, vec![edge("c", EdgeKind::Blocks, "a")]);
    assert_eq!(line.archived_at, at(100));
    assert_eq!(
        line.summary,
        "Find hotels near the venue  research  done; worker pinch"
    );

    assert_eq!(
        store
            .work_archive_compact(&["c".into()], at(200))
            .await
            .unwrap(),
        0,
        "already archived is skipped"
    );
    assert_eq!(
        store
            .work_archive_list(Some(WorkKind::Research), None)
            .await
            .unwrap(),
        vec![line.clone()]
    );
    assert!(store
        .work_archive_list(Some(WorkKind::Code), None)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        store.work_archive_search("HOTELS").await.unwrap(),
        vec![line]
    );
    assert!(store.work_archive_search("%").await.unwrap().is_empty());
}

#[test]
fn migrations_add_work_item_id_once() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE scheduled_jobs (
             id TEXT PRIMARY KEY, schedule TEXT NOT NULL, task TEXT NOT NULL,
             channel TEXT, chat_id TEXT, one_shot INTEGER NOT NULL DEFAULT 0,
             enabled INTEGER NOT NULL DEFAULT 1, next_run_at TEXT NOT NULL,
             last_run_at TEXT, created_at TEXT NOT NULL
         );
         CREATE TABLE delegated_tasks (
             id TEXT PRIMARY KEY, message TEXT NOT NULL, conversation_id TEXT,
             status TEXT NOT NULL, result TEXT, error TEXT, principal TEXT,
             hop_budget INTEGER NOT NULL DEFAULT 0, allowed_tools TEXT,
             trace_id TEXT, created_at TEXT NOT NULL, started_at TEXT,
             finished_at TEXT
         );
         INSERT INTO delegated_tasks (id, message, status, created_at)
             VALUES ('t', 'm', 'queued', 't');",
    )
    .unwrap();

    for _ in 0..3 {
        Store::run_migrations(&conn).unwrap();
    }
    for table in ["scheduled_jobs", "delegated_tasks"] {
        let n: i64 = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = 'work_item_id'"
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "{table}");
    }
    let linked: Option<String> = conn
        .query_row(
            "SELECT work_item_id FROM delegated_tasks WHERE id = 't'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(linked, None, "an existing task was never a work item");

    // And a fresh database is left alone by a second and third run.
    let fresh = rusqlite::Connection::open_in_memory().unwrap();
    for _ in 0..3 {
        Store::run_migrations(&fresh).unwrap();
    }
}

#[tokio::test]
async fn an_unknown_status_reads_as_failed_and_is_final() {
    let store = seeded(
        vec![item("a", Status::Queued), item("b", Status::Queued)],
        vec![],
    )
    .await;
    sql(
        &store,
        "UPDATE work_items SET status = 'paused' WHERE id = 'a'",
    );
    sql(
        &store,
        "UPDATE work_items SET status = 'blocked', status_reason = NULL WHERE id = 'b'",
    );

    assert_eq!(status_of(&store, "a").await, Status::Failed);
    assert_eq!(status_of(&store, "b").await, Status::Failed);
    let open: Vec<(String, Status)> = store
        .work_open_items()
        .await
        .unwrap()
        .into_iter()
        .map(|i| (i.id, i.status))
        .collect();
    assert_eq!(
        open,
        [
            ("a".to_string(), Status::Failed),
            ("b".to_string(), Status::Failed)
        ],
        "still in the snapshot, so the controller holds what depends on them"
    );
    assert!(matches!(
        store.work_transition_many(&[to("a", Status::Ready)]).await,
        Err(WorkStoreError::Closed { .. })
    ));
}

#[tokio::test]
async fn an_unreadable_column_reads_the_item_as_failed() {
    let store = seeded(
        vec![item("a", Status::Ready), item("b", Status::Ready)],
        vec![],
    )
    .await;
    sql(
        &store,
        "UPDATE work_items SET preconditions = 'not json' WHERE id = 'a'",
    );
    sql(
        &store,
        "UPDATE work_items SET kind = 'chore' WHERE id = 'b'",
    );
    assert_eq!(status_of(&store, "a").await, Status::Failed);
    assert_eq!(status_of(&store, "b").await, Status::Failed);

    // An edge kind the store cannot read orders work as strictly as it can.
    sql(
        &store,
        "INSERT INTO work_item_deps (item, depends_on, kind) VALUES ('a', 'b', 'someday')",
    );
    assert_eq!(
        store.work_edges_of("a").await.unwrap(),
        vec![edge("a", EdgeKind::Blocks, "b")]
    );
}

#[tokio::test]
async fn the_outbox_delivers_each_notice_once() {
    let store = Store::open_in_memory();
    let first = store
        .work_outbox_enqueue("p", Some("c"), "telegram", "1 of 2 done")
        .await
        .unwrap();
    let second = store
        .work_outbox_enqueue("p", None, "telegram", "2 of 2 done")
        .await
        .unwrap();
    let pending = store.work_outbox_pending().await.unwrap();
    assert_eq!(
        pending.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        [first.as_str(), second.as_str()]
    );
    assert_eq!(pending[0].origin.as_deref(), Some("c"));

    assert!(store.work_outbox_mark_delivered(&first).await.unwrap());
    assert!(
        !store.work_outbox_mark_delivered(&first).await.unwrap(),
        "a second delivery is reported, not repeated"
    );
    assert_eq!(store.work_outbox_pending().await.unwrap().len(), 1);
    assert!(matches!(
        store.work_outbox_mark_delivered("nope").await,
        Err(WorkStoreError::NotFound(_))
    ));
}

#[tokio::test]
async fn a_batch_lands_whole_or_not_at_all() {
    let store = seeded(
        vec![item("a", Status::Running), item("b", Status::Done)],
        vec![],
    )
    .await;
    let notice = OutboxDraft {
        parent: "a".into(),
        origin: None,
        channel: "telegram".into(),
        body: "done".into(),
    };
    let batch = |tail: Option<WorkOp>| {
        let mut ops = vec![
            WorkOp::Insert(Box::new(item("n", Status::Queued))),
            WorkOp::AddEdge(edge("n", EdgeKind::Blocks, "a")),
            WorkOp::Transition(to("a", Status::Done)),
            WorkOp::Transition(to("n", Status::Ready)),
            WorkOp::Outbox(notice.clone()),
        ];
        ops.extend(tail);
        ops
    };

    let refused = store
        .work_apply(batch(Some(WorkOp::Transition(to("b", Status::Ready)))))
        .await;
    assert!(matches!(refused, Err(WorkStoreError::Closed { .. })));
    assert_eq!(store.work_get("n").await.unwrap(), None);
    assert_eq!(status_of(&store, "a").await, Status::Running);
    assert!(store.work_outbox_pending().await.unwrap().is_empty());

    let applied = store.work_apply(batch(None)).await.unwrap();
    assert_eq!(applied.events.len(), 2);
    assert_eq!(applied.outbox_ids.len(), 1);
    assert_eq!(status_of(&store, "n").await, Status::Ready);
    assert_eq!(status_of(&store, "a").await, Status::Done);
}

#[tokio::test]
async fn a_note_event_may_not_carry_a_status() {
    let store = seeded(vec![item("a", Status::Running)], vec![]).await;
    let note = WorkEvent {
        item: "a".into(),
        at: at(50),
        kind: EventKind::Rung,
        from: None,
        to: None,
        actor: "controller".into(),
        reason: Some("retry: timeout".into()),
        upstream: None,
        origin: None,
        evidence_ref: None,
    };
    store.work_event_append(&note).await.unwrap();
    let lying = WorkEvent {
        to: Some(Status::Done),
        ..note.clone()
    };
    assert_eq!(
        store.work_event_append(&lying).await,
        Err(WorkStoreError::EventCarriesStatus {
            item: "a".into(),
            kind: EventKind::Rung
        })
    );
    assert_eq!(status_of(&store, "a").await, Status::Running);
    assert_eq!(store.work_events("a").await.unwrap(), vec![note.clone()]);
    assert_eq!(store.work_events_since(at(50)).await.unwrap(), vec![note]);
    assert!(store.work_events_since(at(51)).await.unwrap().is_empty());
}

#[tokio::test]
async fn filters_and_the_open_snapshot() {
    let child = |id: &str, status: Status, kind: WorkKind| WorkItem {
        parent: Some("p".into()),
        kind,
        ..item(id, status)
    };
    let store = seeded(
        vec![
            item("p", Status::Running),
            child("q", Status::Queued, WorkKind::Personal),
            child("r", Status::Ready, WorkKind::Code),
            child("d", Status::Done, WorkKind::Personal),
            item("b", Status::Blocked(BlockedReason::NeedsTool)),
            item("x", Status::Done),
        ],
        vec![
            edge("r", EdgeKind::Blocks, "d"),
            edge("d", EdgeKind::Blocks, "x"),
            edge("x", EdgeKind::DiscoveredFrom, "q"),
        ],
    )
    .await;
    let ids = |items: Vec<WorkItem>| items.into_iter().map(|i| i.id).collect::<Vec<_>>();

    assert_eq!(
        ids(store.work_open_items().await.unwrap()),
        ["b", "p", "q", "r"]
    );
    let all = WorkFilter {
        include_closed: true,
        ..WorkFilter::default()
    };
    assert_eq!(store.work_list(&all).await.unwrap().len(), 6);
    let blocked = |r| WorkFilter {
        status: Some(Status::Blocked(r)),
        ..WorkFilter::default()
    };
    assert_eq!(
        ids(store
            .work_list(&blocked(BlockedReason::NeedsTool))
            .await
            .unwrap()),
        ["b"]
    );
    assert!(store
        .work_list(&blocked(BlockedReason::NeedsDecision))
        .await
        .unwrap()
        .is_empty());
    let done = WorkFilter {
        status: Some(Status::Done),
        ..WorkFilter::default()
    };
    assert_eq!(
        ids(store.work_list(&done).await.unwrap()),
        ["d", "x"],
        "an explicit status overrides include_closed"
    );
    let code = WorkFilter {
        kind: Some(WorkKind::Code),
        ..WorkFilter::default()
    };
    assert_eq!(ids(store.work_list(&code).await.unwrap()), ["r"]);
    assert_eq!(
        ids(store.work_children("p").await.unwrap()),
        ["d", "q", "r"]
    );

    // Every edge with an open item at either end; `d -> x` has none.
    assert_eq!(
        store.work_edges_all_open().await.unwrap(),
        vec![
            edge("r", EdgeKind::Blocks, "d"),
            edge("x", EdgeKind::DiscoveredFrom, "q"),
        ]
    );
}

#[tokio::test]
async fn a_filing_is_accepted_whole_or_not_at_all() {
    let plan = WorkPlanRow {
        id: "plan-1".into(),
        root: "root".into(),
        filed_by: Some("planner-item".into()),
        rationale: "two independent bookings".into(),
        approval_question: Some("q-7".into()),
        policy: Some("payments".into()),
        created_at: at(30),
    };
    let store = Store::open_in_memory();
    store
        .work_insert_graph(
            &[item("root", Status::Queued), item("a", Status::Queued)],
            &[edge("a", EdgeKind::WaitsFor, "root")],
            Some(&plan),
        )
        .await
        .unwrap();
    assert_eq!(store.work_plan_get("plan-1").await.unwrap(), Some(plan));

    // A second filing that reuses a live id is refused, and nothing from it
    // survives: not its new item, not its edge.
    let refused = store
        .work_insert_graph(
            &[item("fresh", Status::Queued), item("a", Status::Queued)],
            &[edge("fresh", EdgeKind::Blocks, "a")],
            None,
        )
        .await;
    assert_eq!(
        refused,
        Err(WorkStoreError::AlreadyExists("work item a".into()))
    );
    assert_eq!(store.work_get("fresh").await.unwrap(), None);
    assert!(store.work_dependents_of("a").await.unwrap().is_empty());

    // An edge whose downstream is not live is refused, too.
    assert!(matches!(
        store
            .work_add_edges(&[edge("ghost", EdgeKind::Blocks, "a")])
            .await,
        Err(WorkStoreError::NotFound(_))
    ));
}

#[tokio::test]
async fn the_event_cursor_returns_each_row_once_in_write_order() {
    let store = seeded(vec![item("a", Status::Running)], vec![]).await;
    let start = store.work_events_last_id().await.unwrap();
    let note = |n: i64| WorkEvent {
        item: "a".into(),
        // One timestamp for all three: a time cursor could not tell them
        // apart, the row id can.
        at: at(60),
        kind: EventKind::Warning,
        from: None,
        to: None,
        actor: "controller".into(),
        reason: Some(format!("note {n}")),
        upstream: None,
        origin: None,
        evidence_ref: None,
    };
    for n in 0..3 {
        store.work_event_append(&note(n)).await.unwrap();
    }
    let first = store.work_events_after(start, 2).await.unwrap();
    assert_eq!(
        first.iter().map(|(_, e)| e.clone()).collect::<Vec<_>>(),
        vec![note(0), note(1)]
    );
    let cursor = first.last().unwrap().0;
    let rest = store.work_events_after(cursor, 10).await.unwrap();
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0].1, note(2));
    assert_eq!(store.work_events_last_id().await.unwrap(), rest[0].0);
    assert!(store
        .work_events_after(rest[0].0, 10)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn evidence_is_found_by_kind_across_items() {
    let store = seeded(
        vec![item("a", Status::Done), item("b", Status::Running)],
        vec![],
    )
    .await;
    let evidence = |item: &str, kind: &str, reference: &str, secs: i64| Evidence {
        item: item.into(),
        kind: kind.into(),
        reference: reference.into(),
        hash: None,
        verified_by: Some("result_report".into()),
        at: at(secs),
    };
    store
        .work_evidence_add(evidence("b", "classifier_rule", "process: zqx", 2))
        .await
        .unwrap();
    store
        .work_evidence_add(evidence("a", "path", "probe.rs", 1))
        .await
        .unwrap();
    store
        .work_evidence_add(evidence("a", "classifier_rule", "network: flux", 3))
        .await
        .unwrap();
    let rules = store
        .work_evidence_of_kind("classifier_rule")
        .await
        .unwrap();
    let found: Vec<(&str, &str)> = rules
        .iter()
        .map(|e| (e.item.as_str(), e.reference.as_str()))
        .collect();
    assert_eq!(found, [("b", "process: zqx"), ("a", "network: flux")]);
    assert!(store
        .work_evidence_of_kind("nothing")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn an_ended_lease_keeps_its_inputs_in_the_history() {
    let store = seeded(vec![item("a", Status::Ready)], vec![]).await;
    let input = InputRef {
        item: "up".into(),
        title: "upstream".into(),
        status: Status::Done,
        edge: None,
        evidence: vec![ArtifactRef {
            kind: "path".into(),
            value: "notes.md".into(),
        }],
        artifacts: vec![],
        summary: "one line".into(),
        error: None,
    };
    store
        .work_lease_acquire("a", "pinch", 600, vec![input.clone()])
        .await
        .unwrap();
    // Returned to ready (a lost run), leased again, then closed.
    store
        .work_transition_many(&[to("a", Status::Ready)])
        .await
        .unwrap();
    store
        .work_lease_acquire("a", "krabby", 600, vec![])
        .await
        .unwrap();
    let live = store.work_lease_history("a").await.unwrap();
    assert_eq!(live.len(), 2);
    assert_eq!(live[0].lease.worker, "pinch");
    assert_eq!(live[0].lease.inputs, vec![input.clone()]);
    assert!(live[0].released_at.is_some());
    assert_eq!(live[1].lease.worker, "krabby");
    assert_eq!(live[1].released_at, None, "the live lease comes last");

    store
        .work_transition_many(&[to("a", Status::Running), to("a", Status::Done)])
        .await
        .unwrap();
    assert_eq!(store.work_lease_get("a").await.unwrap(), None);
    let ended = store.work_lease_history("a").await.unwrap();
    assert_eq!(ended.len(), 2);
    assert!(ended.iter().all(|r| r.released_at.is_some()));
    assert_eq!(ended[0].lease.inputs, vec![input.clone()]);

    // Compaction keeps the history, as it keeps events and evidence.
    store
        .work_archive_compact(&["a".into()], at(9))
        .await
        .unwrap();
    assert_eq!(store.work_lease_history("a").await.unwrap().len(), 2);

    // An explicit release ends the lease into the history too.
    let store = seeded(vec![item("b", Status::Ready)], vec![]).await;
    store
        .work_lease_acquire("b", "pinch", 60, vec![])
        .await
        .unwrap();
    assert!(store.work_lease_release("b").await.unwrap().is_some());
    let released = store.work_lease_history("b").await.unwrap();
    assert_eq!(released.len(), 1);
    assert!(released[0].released_at.is_some());
}

#[tokio::test]
async fn spend_sums_per_item_and_fills_the_archive_cost() {
    let store = seeded(
        vec![
            item("a", Status::Done),
            item("b", Status::Done),
            item("c", Status::Cancelled(CancelReason::Requested)),
        ],
        vec![],
    )
    .await;
    let run = |item: &str, tokens: u64, wall_ms: u64, iterations: u32| RunSpend {
        item: item.into(),
        run: Some(format!("run-{item}-{tokens}")),
        worker: "pinch".into(),
        tokens,
        wall_ms,
        iterations,
        at: at(5),
    };
    store
        .work_spend_record(run("a", 1_000, 2_500, 3))
        .await
        .unwrap();
    store
        .work_spend_record(run("a", 500, 500, 1))
        .await
        .unwrap();
    store.work_spend_record(run("b", 70, 10, 1)).await.unwrap();
    // A run may end after its item was archived.
    store.work_spend_record(run("gone", 1, 1, 1)).await.unwrap();

    let a = store.work_spend_of("a").await.unwrap();
    assert_eq!(
        a,
        Spend {
            runs: 2,
            tokens: 1_500,
            wall_ms: 3_000,
            iterations: 4
        }
    );
    assert_eq!(store.work_spend_of("c").await.unwrap(), Spend::default());
    let totals = store.work_spend_totals().await.unwrap();
    assert_eq!(totals.get("a"), Some(&a));
    assert_eq!(totals.get("b").map(|s| s.tokens), Some(70));
    assert_eq!(totals.get("c"), None);

    store
        .work_archive_compact(&["a".into(), "c".into()], at(9))
        .await
        .unwrap();
    let archived = store.work_archive_get("a").await.unwrap().unwrap();
    let cost: Spend = serde_json::from_value(archived.cost.expect("a cost")).unwrap();
    assert_eq!(cost, a);
    let never_ran = store.work_archive_get("c").await.unwrap().unwrap();
    assert_eq!(never_ran.cost, None, "no run recorded spend");
}

#[tokio::test]
async fn releasing_a_hold_clears_held_by_in_a_batch() {
    let mut held = item("a", Status::Blocked(BlockedReason::NeedsConsent));
    held.held_by = Some("q-1".into());
    let store = seeded(vec![held], vec![]).await;
    store
        .work_apply(vec![
            WorkOp::Transition(TransitionSpec {
                reason: Some("approved q-1".into()),
                ..to("a", Status::Queued)
            }),
            WorkOp::ReleaseHold("a".into()),
        ])
        .await
        .unwrap();
    let row = store.work_get("a").await.unwrap().unwrap();
    assert_eq!(row.held_by, None);
    assert_eq!(row.status, Status::Queued);
    assert!(matches!(
        store.work_release_hold("missing").await,
        Err(WorkStoreError::NotFound(_))
    ));
    store.work_release_hold("a").await.unwrap();
}

#[tokio::test]
async fn adding_an_artifact_ref_appends_once_and_refuses_unknown_ids() {
    let mut seeded_item = item("a", Status::Running);
    seeded_item.artifact_refs = vec![ArtifactRef {
        kind: "path".into(),
        value: "src/lib.rs".into(),
    }];
    let store = seeded(vec![seeded_item], vec![]).await;
    let commit = ArtifactRef {
        kind: "commit".into(),
        value: "1d1608b".into(),
    };
    let add = |id: &str, artifact: &ArtifactRef| WorkOp::AddArtifactRef {
        item: id.into(),
        artifact: artifact.clone(),
    };

    // Appends after the refs already there, keeping their order.
    store.work_apply(vec![add("a", &commit)]).await.unwrap();
    let refs = store.work_get("a").await.unwrap().unwrap().artifact_refs;
    assert_eq!(refs.len(), 2);
    assert_eq!(refs[1], commit);

    // An identical ref is a no-op, not a second copy.
    store.work_apply(vec![add("a", &commit)]).await.unwrap();
    assert_eq!(
        store.work_get("a").await.unwrap().unwrap().artifact_refs,
        refs
    );

    // An unknown id is refused, and the whole batch rolls back with it.
    let other = ArtifactRef {
        kind: "url".into(),
        value: "https://example.com".into(),
    };
    assert!(matches!(
        store
            .work_apply(vec![add("a", &other), add("missing", &other)])
            .await,
        Err(WorkStoreError::NotFound(_))
    ));
    assert_eq!(
        store.work_get("a").await.unwrap().unwrap().artifact_refs,
        refs
    );
}

#[tokio::test]
async fn monitoring_is_bounded_keeps_counts_and_replays_every_later_event() {
    let store = seeded(
        vec![
            item("waiting", Status::Queued),
            item("active", Status::Ready),
            item("old", Status::Done),
        ],
        vec![],
    )
    .await;
    store
        .work_lease_acquire("active", "pinch", 90, vec![])
        .await
        .unwrap();
    store
        .work_evidence_add(Evidence {
            item: "active".into(),
            kind: "command".into(),
            reference: "cargo test".into(),
            hash: None,
            verified_by: Some("controller".into()),
            at: Utc::now(),
        })
        .await
        .unwrap();
    let snap = store.work_monitor_snapshot(1, 1).await.unwrap();
    assert_eq!(snap.total_live, 3);
    assert_eq!(snap.counts["leased"], 1);
    assert_eq!(snap.active_by_worker["pinch"], 1);
    assert_eq!(snap.counts["queued"], 1);
    assert!(snap.items_truncated);
    assert_eq!(snap.items[0].item.id, "active");
    assert_eq!(snap.items[0].lease.as_ref().unwrap().worker, "pinch");
    assert_eq!(snap.items[0].verified_evidence_count, 1);
    assert_eq!(snap.events.len(), 1);
    assert_eq!(snap.events[0].cursor, snap.event_cursor);
    // Observation adds no event and changes no item or lease.
    assert_eq!(
        store.work_events_last_id().await.unwrap(),
        snap.event_cursor
    );
    assert_eq!(
        store.work_get("active").await.unwrap().unwrap().status,
        Status::Leased
    );
    store
        .work_event_append(&WorkEvent {
            item: "active".into(),
            at: Utc::now(),
            kind: EventKind::Warning,
            from: None,
            to: None,
            actor: "controller".into(),
            reason: Some("later".into()),
            upstream: None,
            origin: None,
            evidence_ref: None,
        })
        .await
        .unwrap();
    let later = store
        .work_events_after(snap.event_cursor, 10)
        .await
        .unwrap();
    assert_eq!(later.len(), 1);
    assert_eq!(later[0].1.reason.as_deref(), Some("later"));
}

#[tokio::test]
async fn a_handoff_receipt_and_lease_roll_back_together() {
    let store = seeded(vec![item("work", Status::Ready)], vec![]).await;
    let ev = Evidence {
        item: "another-item".into(),
        kind: "project_context".into(),
        reference: "{}".into(),
        hash: None,
        verified_by: Some("controller".into()),
        at: at(0),
    };
    assert!(store
        .work_lease_acquire_recorded("work", "codex", 60, vec![], vec![ev])
        .await
        .is_err());
    assert_eq!(status_of(&store, "work").await, Status::Ready);
    assert!(store.work_lease_get("work").await.unwrap().is_none());
    assert!(store.work_events("work").await.unwrap().is_empty());
}

#[tokio::test]
async fn monitoring_workspace_comes_from_the_controller_not_a_model_artifact() {
    let store = seeded(vec![item("work", Status::Ready)], vec![]).await;
    for (reference, hash, verified_by) in [
        ("{\"base\":\"actual\"}", Some("actual".into()), None),
        ("{\"base\":\"forged\"}", None, Some("result_report".into())),
    ] {
        store
            .work_evidence_add(Evidence {
                item: "work".into(),
                kind: "workspace".into(),
                reference: reference.into(),
                hash,
                verified_by,
                at: at(0),
            })
            .await
            .unwrap();
    }
    let snap = store.work_monitor_snapshot(1, 1).await.unwrap();
    assert_eq!(snap.items[0].workspace.as_ref().unwrap()["base"], "actual");
}
