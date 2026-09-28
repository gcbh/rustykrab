//! A firing end to end: filed as a work item, run by the controller on a
//! local worker in the job's own conversation with its SKILL.md and its
//! scheduled prompt, then recorded as the job's run and the job advanced.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Mutex;

use rustykrab_agent::{LocalWorker, NoSandbox};
use rustykrab_control::controller::{Controller, ControllerConfig, ManualClock};
use rustykrab_core::model::{ModelProvider, ModelResponse, StopReason, Usage};
use rustykrab_core::types::{Message, MessageContent, Role, ToolCall, ToolSchema};
use rustykrab_core::work::Trigger;
use serde_json::json;

use super::*;

/// Replays one response per call and records every request's messages.
struct Replay {
    script: Mutex<VecDeque<ModelResponse>>,
    requests: Mutex<Vec<Vec<Message>>>,
}

impl Replay {
    fn new(script: Vec<ModelResponse>) -> Arc<Replay> {
        Arc::new(Replay {
            script: Mutex::new(script.into()),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn requests(&self) -> Vec<Vec<Message>> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl ModelProvider for Replay {
    fn name(&self) -> &str {
        "replay"
    }

    async fn chat(
        &self,
        messages: &[Message],
        _tools: &[ToolSchema],
    ) -> rustykrab_core::Result<ModelResponse> {
        self.requests.lock().unwrap().push(messages.to_vec());
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| rustykrab_core::Error::ModelEmptyResponse("script spent".into()))
    }
}

fn call(name: &str, args: serde_json::Value) -> ModelResponse {
    ModelResponse {
        message: Message::stamped(
            Role::Assistant,
            MessageContent::ToolCall(ToolCall {
                id: Uuid::new_v4().to_string(),
                name: name.into(),
                arguments: args,
            }),
        ),
        usage: Usage {
            prompt_tokens: 900,
            completion_tokens: 100,
            ..Usage::default()
        },
        stop_reason: StopReason::ToolUse,
        text: None,
    }
}

struct Daemon {
    state: AppState,
    controller: Arc<Controller>,
    provider: Arc<Replay>,
    dir: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn skill(name: &str, body: &str) -> Arc<rustykrab_skills::skill_md::SkillMd> {
    use rustykrab_skills::skill_md::{
        RequirementValidation, SkillMd, SkillMdFrontmatter, SkillRequirements,
    };
    Arc::new(SkillMd {
        path: PathBuf::from(format!("/skills/{name}")),
        frontmatter: SkillMdFrontmatter {
            name: name.to_string(),
            description: format!("{name} description"),
            version: "1.0".to_string(),
            requires: SkillRequirements::default(),
            user_invocable: true,
            emoji: None,
            outcome: None,
            extra: HashMap::new(),
        },
        raw_body: body.to_string(),
        validation: RequirementValidation {
            missing_env: Vec::new(),
            missing_bins: Vec::new(),
        },
    })
}

/// A daemon's worth: store, local worker on `script`, controller on a
/// clock `ahead` of now, and the app state the firing loop delivers from.
fn daemon(script: Vec<ModelResponse>, ahead: TimeDelta) -> Daemon {
    let dir = std::env::temp_dir().join(format!("rk-scheduled-{}", Uuid::new_v4()));
    let store = Store::open(dir.join("db"), vec![7u8; 32]).expect("store opens");
    let skills = Arc::new(SkillRegistry::new());
    skills.register_md(skill(
        "water-plants",
        "Step 1: water the fern. Step 2: water the basil.",
    ));
    let provider = Replay::new(script);
    let deferred = Arc::new(crate::DeferredWorkBackend::default());
    let worker = LocalWorker::new(
        "pinch",
        LocalWorker::default_definition("pinch"),
        provider.clone(),
        Vec::new(),
        Arc::new(NoSandbox),
        deferred.clone(),
    )
    .with_transcripts(transcripts(&store, skills));
    let controller = Arc::new(
        Controller::new(
            store.clone(),
            vec![Arc::new(worker)],
            ControllerConfig::default(),
        )
        .with_clock(Arc::new(ManualClock::new(Utc::now() + ahead))),
    );
    deferred.bind(controller.clone());
    let state = AppState::new(store, Vec::new(), provider.clone(), "token".into())
        .with_control(controller.clone());
    Daemon {
        state,
        controller,
        provider,
        dir,
    }
}

impl Daemon {
    fn store(&self) -> &Store {
        &self.state.agent.store
    }

    async fn poll(&self, now: DateTime<Utc>) {
        poll(
            &self.state,
            self.controller.as_ref(),
            now,
            TimeDelta::seconds(1),
        )
        .await
        .expect("poll");
    }

    async fn run(&self) {
        self.controller
            .run_until_idle(40, Duration::from_millis(500))
            .await
            .expect("controller runs");
    }
}

#[tokio::test]
async fn a_firing_runs_in_the_jobs_conversation_with_its_skill_and_is_recorded_once() {
    let d = daemon(
        vec![call(
            "result_report",
            json!({ "summary": "Watered the fern and the basil." }),
        )],
        TimeDelta::seconds(30),
    );
    let at = Utc::now() + TimeDelta::seconds(1);
    let job = d
        .store()
        .jobs()
        .create_job(
            &at.to_rfc3339(),
            "water-plants on the balcony",
            Some("telegram"),
            Some("4242"),
            Some("101"),
            "UTC",
            true,
        )
        .await
        .unwrap();

    d.poll(job.next_run_at).await;
    let item_id = d
        .store()
        .jobs()
        .work_item_id(&job.id)
        .await
        .unwrap()
        .expect("the firing is a work item");
    let item = d.store().work_get(&item_id).await.unwrap().unwrap();
    assert!(matches!(item.trigger, Trigger::At(_)), "{:?}", item.trigger);
    assert_eq!(
        item.artifact_refs,
        vec![ArtifactRef {
            kind: JOB_REF.into(),
            value: job.id.clone()
        }]
    );
    let conversation = d
        .store()
        .jobs()
        .get_job(&job.id)
        .await
        .unwrap()
        .conversation_id
        .expect("the job's conversation");
    assert_eq!(
        item.origin_conversation_id.as_deref(),
        Some(conversation.as_str())
    );
    // Filed once: a second poll before it runs files nothing more.
    d.poll(job.next_run_at).await;
    assert_eq!(d.store().work_open_items().await.unwrap().len(), 1);

    d.run().await;
    assert_eq!(
        d.store().work_get(&item_id).await.unwrap().unwrap().status,
        Status::Done
    );

    // The run: the job's conversation, one leading system message carrying
    // the skill, and the scheduled prompt ahead of the brief.
    let requests = d.provider.requests();
    assert_eq!(requests.len(), 1);
    let messages = &requests[0];
    let system = messages[0].content.as_text().unwrap();
    assert!(
        system.contains("<skill_instructions name=\"water-plants\">"),
        "{system}"
    );
    assert!(system.contains("Step 2: water the basil."), "{system}");
    assert!(messages[1..].iter().all(|m| m.role != Role::System));
    let turn = messages.last().unwrap().content.as_text().unwrap();
    assert!(
        turn.starts_with("[Scheduled task] Your scheduled task is due for the first time."),
        "{turn}"
    );
    assert!(turn.contains("Task: water-plants on the balcony"), "{turn}");
    assert!(turn.contains("delivered to telegram (4242)"), "{turn}");
    assert!(turn.contains("memory_search"), "{turn}");
    assert!(turn.contains("work item: #"), "{turn}");
    let kept = d
        .store()
        .conversations()
        .get(Uuid::parse_str(&conversation).unwrap())
        .await
        .unwrap();
    assert!(kept
        .messages
        .iter()
        .any(|m| m.content.as_text() == Some(turn)));
    let run = d.store().work_evidence_list(&item_id).await.unwrap();
    assert!(run
        .iter()
        .any(|e| e.kind == "run" && e.reference == conversation));

    // Finished: recorded as the job's run, the one-shot job retired, and
    // nothing filed again.
    d.poll(Utc::now()).await;
    let runs = d.store().jobs().list_runs(&job.id, 10).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, "ok");
    assert_eq!(
        runs[0].output.as_deref(),
        Some("Watered the fern and the basil.")
    );
    let after = d.store().jobs().get_job(&job.id).await.unwrap();
    assert!(!after.enabled, "a one-shot job runs once");
    assert!(after.last_run_at.is_some());
    assert!(finished_with(d.store(), &item_id).await.unwrap());
    d.poll(Utc::now() + TimeDelta::days(2)).await;
    assert_eq!(
        d.store().jobs().list_runs(&job.id, 10).await.unwrap().len(),
        1
    );
    assert!(d.store().work_open_items().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_firing_that_ends_without_a_result_is_an_error_run_and_the_job_advances() {
    let d = daemon(Vec::new(), TimeDelta::seconds(30));
    let job = d
        .store()
        .jobs()
        .create_job(
            "0 9 * * *",
            "Summarise the garden log",
            None,
            None,
            None,
            "UTC",
            false,
        )
        .await
        .unwrap();
    d.poll(job.next_run_at).await;
    let item_id = d
        .store()
        .jobs()
        .work_item_id(&job.id)
        .await
        .unwrap()
        .unwrap();
    // Cancelled before it ran, as a user would: it closes without a result.
    d.controller
        .cancel(&item_id, Some("not today".into()), "user")
        .await
        .unwrap();
    d.poll(job.next_run_at).await;
    let runs = d.store().jobs().list_runs(&job.id, 10).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, "error");
    assert!(
        runs[0]
            .output
            .as_deref()
            .is_some_and(|o| o.contains("cancelled")),
        "{:?}",
        runs[0].output
    );
    let after = d.store().jobs().get_job(&job.id).await.unwrap();
    assert!(after.enabled, "a recurring job keeps firing");
    assert!(after.last_run_at.is_some(), "and has advanced");
}

#[tokio::test]
async fn a_deleted_jobs_open_firing_is_cancelled_and_an_unlinked_one_adopted() {
    let d = daemon(Vec::new(), TimeDelta::seconds(0));
    let at = Utc::now() + TimeDelta::hours(1);
    let job = d
        .store()
        .jobs()
        .create_job(
            &at.to_rfc3339(),
            "Feed the cat",
            None,
            None,
            None,
            "UTC",
            true,
        )
        .await
        .unwrap();
    d.poll(job.next_run_at).await;
    let first = d
        .store()
        .jobs()
        .work_item_id(&job.id)
        .await
        .unwrap()
        .unwrap();

    // The link is lost (a restart between filing and linking): the next
    // poll adopts the open firing instead of filing a second.
    d.store()
        .jobs()
        .set_work_item_id(&job.id, "lost")
        .await
        .unwrap();
    d.poll(job.next_run_at).await;
    assert_eq!(
        d.store()
            .jobs()
            .work_item_id(&job.id)
            .await
            .unwrap()
            .as_deref(),
        Some(first.as_str())
    );
    assert_eq!(d.store().work_open_items().await.unwrap().len(), 1);

    d.store().jobs().delete_job(&job.id).await.unwrap();
    d.poll(Utc::now()).await;
    assert!(d
        .store()
        .work_get(&first)
        .await
        .unwrap()
        .unwrap()
        .status
        .is_closed());
}

#[test]
fn the_firing_prompt_says_where_the_summary_goes() {
    let to = firing_prompt("Daily briefing.", None, Some("telegram"), Some("1"));
    assert!(to.starts_with("[Scheduled task] Your scheduled task is due for the first time."));
    assert!(to.contains("Task: Daily briefing."));
    assert!(to.contains("do not have the conversation this job was created in"));
    assert!(to.contains("result_report summary will be delivered to telegram (1)"));
    let last = Utc::now();
    let again = firing_prompt("Daily briefing.", Some(last), None, None);
    assert!(again.contains("due again"));
    assert!(again.contains("result_report summary IS the deliverable"));
}
