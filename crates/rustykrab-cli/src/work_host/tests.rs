//! Where a notice goes, the busy signal, and the stub switch keeping the
//! work tools.

use std::path::PathBuf;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::Utc;
use rustykrab_core::work::{Budget, Trigger, WorkItem, WorkKind, WorkerKind};
use serde_json::Value;

use super::*;

struct TempStore {
    store: Store,
    dir: PathBuf,
}

impl Drop for TempStore {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn temp_store() -> TempStore {
    let dir = std::env::temp_dir().join(format!("rk-work-host-{}", Uuid::new_v4()));
    let store = Store::open(dir.join("db"), vec![5u8; 32]).expect("store opens");
    TempStore { store, dir }
}

fn item(id: &str, status: Status, origin: Option<String>) -> WorkItem {
    let now = Utc::now();
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
        origin_conversation_id: origin,
        trigger: Trigger::Now,
        preconditions: vec![],
        expires_at: None,
        budget: Budget::default(),
        priority: 0,
        status,
        status_origin: None,
        plan_id: None,
        held_by: None,
        created_at: now,
        updated_at: now,
        closed_at: status.is_closed().then_some(now),
    }
}

/// A conversation bound to `address`.
async fn bound(store: &Store, address: &ChannelAddress) -> String {
    let conv = store.conversations().create().await.unwrap();
    store
        .channel_bindings()
        .bind(address, conv.id)
        .await
        .unwrap();
    conv.id.to_string()
}

async fn notice(store: &Store, item: WorkItem, channel: &str) -> OutboxRow {
    let id = item.id.clone();
    store.work_insert_graph(&[item], &[], None).await.unwrap();
    store
        .work_outbox_enqueue(&id, Some(&id), channel, "a notice")
        .await
        .unwrap();
    store
        .work_outbox_pending()
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.parent == id)
        .unwrap()
}

#[tokio::test]
async fn a_notice_goes_to_the_thread_its_item_came_from() {
    let t = temp_store();
    let s = &t.store;
    let telegram = bound(
        s,
        &ChannelAddress::Telegram {
            chat_id: 4242,
            thread_id: 101,
        },
    )
    .await;
    let row = notice(s, item("a", Status::Done, Some(telegram)), "telegram").await;
    assert_eq!(
        route(s, &row, Some("999")).await,
        Route::Send {
            channel: "telegram".into(),
            chat: Some("4242".into()),
            thread: Some("101".into()),
        }
    );

    let general = bound(
        s,
        &ChannelAddress::Telegram {
            chat_id: 4242,
            thread_id: 0,
        },
    )
    .await;
    let row = notice(s, item("b", Status::Done, Some(general)), "telegram").await;
    assert_eq!(
        route(s, &row, Some("999")).await,
        Route::Send {
            channel: "telegram".into(),
            chat: Some("4242".into()),
            thread: None,
        },
        "the General topic is no thread"
    );

    // Bound on another channel: the notice follows the binding.
    let slack = bound(
        s,
        &ChannelAddress::Slack {
            team_id: "T1".into(),
            channel_id: "C9".into(),
            thread_ts: "1712345678.000100".into(),
        },
    )
    .await;
    let row = notice(s, item("c", Status::Failed, Some(slack)), "telegram").await;
    assert_eq!(
        route(s, &row, Some("999")).await,
        Route::Send {
            channel: "slack".into(),
            chat: Some("C9".into()),
            thread: Some("1712345678.000100".into()),
        }
    );

    // No binding (a web chat, or nothing at all): the first allowed chat.
    let web = s.conversations().create().await.unwrap().id.to_string();
    for (id, origin) in [("d", Some(web)), ("e", None)] {
        let row = notice(s, item(id, Status::Done, origin), "telegram").await;
        assert_eq!(
            route(s, &row, Some("999")).await,
            Route::Send {
                channel: "telegram".into(),
                chat: Some("999".into()),
                thread: None,
            }
        );
    }
    let row = notice(s, item("f", Status::Done, None), "webchat").await;
    assert_eq!(
        route(s, &row, Some("999")).await,
        Route::Send {
            channel: "webchat".into(),
            chat: None,
            thread: None,
        }
    );
}

#[tokio::test]
async fn a_firings_notice_goes_to_its_jobs_target_and_a_done_one_is_its_delivery() {
    let t = temp_store();
    let s = &t.store;
    let job = s
        .jobs()
        .create_job(
            "0 9 * * *",
            "Water the plants",
            Some("slack"),
            Some("C77"),
            Some("1700000000.000100"),
            "UTC",
            false,
        )
        .await
        .unwrap();
    let conv = s.conversations().create().await.unwrap().id.to_string();
    s.jobs().set_conversation_id(&job.id, &conv).await.unwrap();

    let row = notice(
        s,
        item("blocked", Status::Failed, Some(conv.clone())),
        "telegram",
    )
    .await;
    s.jobs().set_work_item_id(&job.id, "blocked").await.unwrap();
    assert_eq!(
        route(s, &row, Some("999")).await,
        Route::Send {
            channel: "slack".into(),
            chat: Some("C77".into()),
            thread: Some("1700000000.000100".into()),
        }
    );

    let row = notice(s, item("done", Status::Done, Some(conv)), "telegram").await;
    s.jobs().set_work_item_id(&job.id, "done").await.unwrap();
    assert_eq!(route(s, &row, Some("999")).await, Route::Consumed);
}

/// One send: channel, text, chat, thread.
type Sending = (String, String, Option<String>, Option<String>);

/// Records sends; fails every send to `down`.
#[derive(Default)]
struct Sent {
    sent: Mutex<Vec<Sending>>,
    down: Option<&'static str>,
}

#[async_trait]
impl MessageBackend for Sent {
    async fn send_message(
        &self,
        channel: &str,
        text: &str,
        chat_id: Option<&str>,
        thread_id: Option<&str>,
    ) -> rustykrab_core::Result<Value> {
        if self.down == Some(channel) {
            return Err(rustykrab_core::Error::Internal(format!(
                "{channel} is down"
            )));
        }
        self.sent.lock().unwrap().push((
            channel.into(),
            text.into(),
            chat_id.map(str::to_string),
            thread_id.map(str::to_string),
        ));
        Ok(Value::Null)
    }
}

#[tokio::test]
async fn a_notice_is_marked_delivered_only_once_it_is_sent() {
    let t = temp_store();
    let s = &t.store;
    let thread = bound(
        s,
        &ChannelAddress::Telegram {
            chat_id: 4242,
            thread_id: 7,
        },
    )
    .await;
    notice(s, item("a", Status::Done, Some(thread)), "telegram").await;

    let down = Sent {
        down: Some("telegram"),
        ..Sent::default()
    };
    deliver_pending(s, &down, Some("999")).await;
    assert_eq!(s.work_outbox_pending().await.unwrap().len(), 1, "kept");

    let up = Sent::default();
    deliver_pending(s, &up, Some("999")).await;
    assert_eq!(
        up.sent.lock().unwrap().clone(),
        vec![(
            "telegram".to_string(),
            "a notice".to_string(),
            Some("4242".to_string()),
            Some("7".to_string())
        )]
    );
    assert!(s.work_outbox_pending().await.unwrap().is_empty());
    deliver_pending(s, &up, Some("999")).await;
    assert_eq!(up.sent.lock().unwrap().len(), 1, "never twice");
}

#[test]
fn an_interactive_turn_makes_the_daemons_model_busy() {
    let tracker = ActivityTracker::new();
    let activity = turn_activity(tracker.clone(), "ollama");
    assert!(!activity.busy("ollama"));
    let turn = tracker.begin_run(Uuid::new_v4());
    assert!(activity.busy("ollama"));
    assert!(!activity.busy("another-model"));
    drop(turn);
    assert!(!activity.busy("ollama"));
}

#[test]
fn replace_stubs_keep_the_work_tools_and_a_stub_of_one_wins() {
    let backend: Arc<dyn WorkBackend> = Arc::new(rustykrab_tools::StubWorkBackend::new());
    let real: Vec<Arc<dyn Tool>> = vec![
        Arc::new(rustykrab_tools::TaskCompleteTool::new()),
        Arc::new(rustykrab_tools::TaskCompleteTool::new()),
    ];
    let stubs: rustykrab_tools::StubFile = serde_json::from_value(serde_json::json!({
        "mode": "replace",
        "tools": [{
            "name": "work_status",
            "description": "scripted",
            "parameters": { "type": "object", "properties": {} },
            "script": { "responses": [{ "type": "ok", "value": {} }] }
        }]
    }))
    .unwrap();
    let mut tools = stubs.apply(real);
    add_work_tools(&mut tools, backend);
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    for name in ["work_file", "work_status", "result_report"] {
        assert_eq!(
            names.iter().filter(|n| **n == name).count(),
            1,
            "{name} once in {names:?}"
        );
    }
    let status = tools.iter().find(|t| t.name() == "work_status").unwrap();
    assert_eq!(status.description(), "scripted", "the stub is kept");
}

#[tokio::test]
async fn background_dreaming_notices_stay_in_the_dashboard_without_external_sends() {
    use rustykrab_core::work::{ArtifactRef, BlockedReason};
    let t = temp_store();
    let s = &t.store;
    let review_refs = vec![
        ArtifactRef {
            kind: rustykrab_core::dream_review::REVIEW_ONLY.into(),
            value: "true".into(),
        },
        ArtifactRef {
            kind: "dream_review_job".into(),
            value: "cycle:generator".into(),
        },
    ];
    let mut review = item("review", Status::Done, None);
    review.kind = WorkKind::Research;
    review.artifact_refs = review_refs.clone();
    let review_row = notice(s, review, "telegram").await;
    assert_eq!(route(s, &review_row, Some("999")).await, Route::Consumed);
    let mut proposal = item(
        "proposal",
        Status::Blocked(BlockedReason::NeedsConsent),
        None,
    );
    proposal.kind = WorkKind::Proposal;
    proposal.artifact_refs.push(ArtifactRef {
        kind: "dream_project_review".into(),
        value: "cycle".into(),
    });
    let proposal_row = notice(s, proposal, "telegram").await;
    assert_eq!(route(s, &proposal_row, Some("999")).await, Route::Consumed);
    let backend = Sent::default();
    deliver_pending(s, &backend, Some("999")).await;
    assert!(backend.sent.lock().unwrap().is_empty());
    assert!(s.work_outbox_pending().await.unwrap().is_empty());
    assert_eq!(
        s.work_get("proposal").await.unwrap().unwrap().status,
        Status::Blocked(BlockedReason::NeedsConsent)
    );
    assert!(s.work_outbox_latest("review").await.unwrap().is_some());

    // Read-only research alone is ordinary work, and a conversation explicitly
    // attached to an internal-looking item still receives its own notice.
    let mut ordinary = item("ordinary", Status::Done, None);
    ordinary.kind = WorkKind::Research;
    ordinary.artifact_refs = vec![review_refs[0].clone()];
    let row = notice(s, ordinary, "telegram").await;
    assert!(matches!(
        route(s, &row, Some("999")).await,
        Route::Send { .. }
    ));
    let conversation = bound(
        s,
        &ChannelAddress::Telegram {
            chat_id: 4242,
            thread_id: 7,
        },
    )
    .await;
    let mut explicit = item("explicit", Status::Done, Some(conversation));
    explicit.kind = WorkKind::Research;
    explicit.artifact_refs = review_refs;
    let row = notice(s, explicit, "telegram").await;
    assert_eq!(
        route(s, &row, Some("999")).await,
        send("telegram", Some("4242".into()), Some("7".into()))
    );
}
