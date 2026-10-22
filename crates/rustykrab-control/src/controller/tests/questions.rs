//! Phase 4 against a real store: questions and their routes, answers that
//! resume the item, standing judgment, approval thresholds, the planner, the
//! re-plan, stalls and timed notices (plan sections 6.1, 6.4 to 6.6, 7).

use std::sync::{Arc, Mutex, OnceLock};

use chrono::TimeDelta;
use rustykrab_core::questions::{QuestionClass, QuestionKind, QuestionStatus};
use rustykrab_core::work::{
    BlockedReason, BlockedReport, CancelReason, EdgeKind, ErrorSubclass, Question, ResultReport,
    Rung, Status, WorkKind,
};
use rustykrab_store::{QuestionFilter, QuestionRow};
use rustykrab_tools::work_backend::{AskRequest, Provenance, WorkBackend, WORK_RUN_CONTEXT};
use tokio::sync::Notify;

use super::*;
use crate::handle::ControlHandle;
use crate::questions::baseline;

fn asking(text: &str, class: &str, options: &[&str]) -> ResultReport {
    ResultReport {
        summary: "I need a decision.".to_string(),
        blocked: Some(BlockedReport {
            reason: BlockedReason::NeedsDecision,
            detail: "a choice".to_string(),
            needs: Vec::new(),
        }),
        questions: vec![Question {
            text: text.to_string(),
            class: class.to_string(),
            options: options.iter().map(|o| o.to_string()).collect(),
        }],
        ..ResultReport::default()
    }
}

async fn questions_of(h: &Harness, item: &str) -> Vec<QuestionRow> {
    let mut rows = h
        .store()
        .questions_list(&QuestionFilter {
            item: Some(item.to_string()),
            ..QuestionFilter::default()
        })
        .await
        .unwrap();
    rows.reverse();
    rows
}

// ── scenario 3: park, ask, resume the item ─────────────────────────────

#[tokio::test]
async fn a_blocking_question_parks_asks_once_and_its_answer_resumes_the_item() {
    let h = Harness::new(&["pinch"]);
    h.script.push(
        "Order flowers",
        report(asking(
            "Which florist, Petals or Stems?",
            "blocking_now",
            &["Petals", "Stems"],
        )),
    );
    let x = h.file_one(draft("x", "Order flowers")).await;
    h.drain().await;
    assert_eq!(
        h.status(&x).await,
        Status::Blocked(BlockedReason::NeedsDecision)
    );
    let asked = questions_of(&h, &x).await;
    assert_eq!(asked.len(), 1);
    let q = &asked[0];
    assert_eq!(q.class, QuestionClass::BlockingNow);
    assert_eq!(q.status, QuestionStatus::Open);
    assert_eq!(q.asked_class.as_deref(), Some("blocking_now"));
    assert!(
        q.delivered_via.is_some(),
        "report-carried questions keep the same notification audit as ask_user"
    );
    let outbox = h.outbox().await;
    assert_eq!(outbox.len(), 1, "one message: {outbox:#?}");
    assert!(outbox[0]
        .body
        .contains("Asked: Which florist, Petals or Stems?"));

    // The answer, by the short id the message printed and an option number.
    let short: String = q.id.chars().take(8).collect();
    let reply = ControlHandle::answer(&h.ctl, &short, "2", "user:master")
        .await
        .unwrap();
    assert_eq!(reply.resumed, vec![x.clone()]);
    assert_eq!(reply.question.answer.as_deref(), Some("Stems"));
    assert_eq!(reply.question.status, QuestionStatus::Answered);
    // Answered once only.
    assert!(ControlHandle::answer(&h.ctl, &q.id, "1", "user:master")
        .await
        .is_err());

    h.drain().await;
    assert_eq!(h.status(&x).await, Status::Done);
    let briefs = h.script.briefs_for("Order flowers");
    assert_eq!(briefs.len(), 2);
    assert!(
        briefs[1]
            .decisions_made
            .iter()
            .any(|d| d.contains("answered Stems") && d.contains("the user")),
        "the resumed brief carries the answer: {:?}",
        briefs[1].decisions_made
    );
}

#[tokio::test]
async fn a_defaultable_question_is_answered_by_its_default_and_never_reaches_the_user() {
    let h = Harness::new(&["pinch"]);
    h.script.push(
        "Set the reminder",
        report(asking(
            "Should the reminder use the usual 9am?",
            "defaultable",
            &["9am"],
        )),
    );
    let x = h.file_one(draft("x", "Set the reminder")).await;
    h.drain().await;
    assert_eq!(h.status(&x).await, Status::Done);
    let asked = questions_of(&h, &x).await;
    assert_eq!(asked[0].class, QuestionClass::Defaultable);
    assert_eq!(asked[0].status, QuestionStatus::Defaulted);
    assert_eq!(asked[0].answered_by.as_deref(), Some("default"));
    let briefs = h.script.briefs_for("Set the reminder");
    assert!(briefs[1]
        .decisions_made
        .iter()
        .any(|d| d.contains("answered 9am") && d.contains("recorded default")));
    let outbox = h.outbox().await;
    assert!(
        outbox.iter().all(|o| !o.body.contains("usual 9am")),
        "the defaultable question reached the user: {outbox:#?}"
    );
}

#[tokio::test]
async fn a_question_asked_again_after_its_answer_is_a_loop_for_the_ladder() {
    let h = Harness::new(&["pinch"]);
    for _ in 0..2 {
        h.script.push(
            "Set the alarm",
            report(asking("Use the usual 7am?", "defaultable", &["7am"])),
        );
    }
    let mut d = draft("x", "Set the alarm");
    d.budget = Some(no_rungs());
    let x = h.file_one(d).await;
    h.drain().await;
    let rungs = h.rungs(&x).await;
    assert!(rungs.contains(&Rung::Surface), "{rungs:?}");
    let error = crate::ladder::last_error(&h.events(&x).await).unwrap();
    assert_eq!(error.subclass, ErrorSubclass::Loop);
    assert!(error.detail.contains("asked again"), "{}", error.detail);
    // What surfaced names the loop, not the question.
    let outbox = h.outbox().await;
    assert!(outbox.iter().all(|o| !o.body.contains("usual 7am")));
}

#[tokio::test]
async fn a_granted_topic_is_decided_by_policy_and_the_decision_is_recorded() {
    let h = Harness::new(&["pinch"]);
    ControlHandle::grant_judgment(&h.ctl, "Use your judgment on restaurants.", "", "user")
        .await
        .unwrap();
    h.script.push(
        "Book dinner",
        report(asking(
            "Which restaurant, Tasca or Cervejaria?",
            "blocking_now",
            &["Tasca", "Cervejaria"],
        )),
    );
    let x = h.file_one(draft("x", "Book dinner")).await;
    h.drain().await;
    assert_eq!(h.status(&x).await, Status::Done);
    let asked = questions_of(&h, &x).await;
    assert_eq!(asked[0].status, QuestionStatus::Delegated);
    let decision = asked[0].decision.clone().expect("the record");
    assert_eq!(decision.chosen, "Tasca");
    assert!(!decision.alternatives.is_empty() && !decision.revisit.is_empty());
    assert!(h
        .events(&x)
        .await
        .iter()
        .any(|e| e.kind == EventKind::Decision && e.actor == "policy"));
    assert!(h
        .outbox()
        .await
        .iter()
        .all(|o| !o.body.contains("Which restaurant")));
    // Revoked, the same question would go to the user.
    let view = ControlHandle::judgment(&h.ctl).await.unwrap();
    let id = view.grants[0].id.clone();
    assert!(ControlHandle::revoke_judgment(&h.ctl, &id, "user")
        .await
        .unwrap());
    assert!(ControlHandle::judgment(&h.ctl)
        .await
        .unwrap()
        .grants
        .is_empty());
}

#[tokio::test]
async fn a_researchable_question_files_research_and_the_item_resumes_with_its_answer() {
    let h = Harness::new(&["pinch"]);
    h.script.push(
        "Pick up the prescription",
        report(asking(
            "When does the pharmacy open on Sunday?",
            "researchable",
            &[],
        )),
    );
    h.script.push(
        "Research: When does the pharmacy open on Sunday?",
        report(ResultReport {
            summary: "Opens at 10:00 on Sundays.".to_string(),
            ..done("research")
        }),
    );
    let x = h.file_one(draft("x", "Pick up the prescription")).await;
    h.drain().await;
    let research = h.of_kind(WorkKind::Research).await;
    assert_eq!(research.len(), 1, "one research item");
    assert_eq!(h.status(&research[0].id).await, Status::Done);
    assert_eq!(h.status(&x).await, Status::Done);
    let asked = questions_of(&h, &x).await;
    assert_eq!(asked[0].class, QuestionClass::Researchable);
    assert_eq!(asked[0].status, QuestionStatus::Answered);
    let briefs = h.script.briefs_for("Pick up the prescription");
    assert!(
        briefs[1]
            .decisions_made
            .iter()
            .any(|d| d.contains("Opens at 10:00")),
        "{:?} {:?}",
        briefs[1].decisions_made,
        asked
    );
    assert!(h
        .outbox()
        .await
        .iter()
        .all(|o| !o.body.contains("pharmacy open")));
}

// ── ask_user mid-run ───────────────────────────────────────────────────

#[tokio::test]
async fn ask_user_answers_a_default_at_once_and_parks_on_a_real_decision() {
    let h = Harness::new(&["pinch"]);
    let gate = Arc::new(Notify::new());
    h.script.push(
        "Plan the party",
        Step::Wait(gate.clone(), Box::new(done("Plan the party"))),
    );
    let x = h.file_one(draft("x", "Plan the party")).await;
    h.step().await;
    assert!(h.status(&x).await.is_active());
    let lease = Provenance {
        conversation_id: None,
        filed_by_item: Some(x.clone()),
        actor: "worker:pinch".to_string(),
    };
    let answered = WorkBackend::ask(
        &h.ctl,
        x.clone(),
        AskRequest {
            text: "Use the usual cake shop?".to_string(),
            class: Some("defaultable".to_string()),
            kind: QuestionKind::Decision,
            options: vec!["the usual".to_string()],
            default: None,
        },
        lease.clone(),
    )
    .await
    .unwrap();
    assert_eq!(answered.answer.as_deref(), Some("the usual"));
    assert!(!answered.parked);
    // Not the lease holder: refused.
    let stranger = Provenance {
        filed_by_item: Some("someone-else".to_string()),
        ..lease.clone()
    };
    assert!(WorkBackend::ask(
        &h.ctl,
        x.clone(),
        AskRequest {
            text: "?".to_string(),
            class: None,
            kind: QuestionKind::Decision,
            options: vec![],
            default: None,
        },
        stranger,
    )
    .await
    .is_err());
    let parked = WorkBackend::ask(
        &h.ctl,
        x.clone(),
        AskRequest {
            text: "Saturday or Sunday?".to_string(),
            class: Some("blocking_now".to_string()),
            kind: QuestionKind::Decision,
            options: vec!["Saturday".to_string(), "Sunday".to_string()],
            default: None,
        },
        lease,
    )
    .await
    .unwrap();
    assert!(parked.parked);
    h.tick().await;
    assert_eq!(
        h.status(&x).await,
        Status::Blocked(BlockedReason::NeedsDecision)
    );
    let outbox = h.outbox().await;
    assert_eq!(outbox.len(), 1);
    assert!(outbox[0].body.contains("Asked: Saturday or Sunday?"));
    gate.notify_one();
    ControlHandle::answer(&h.ctl, &parked.question, "Sunday", "user")
        .await
        .unwrap();
    h.drain().await;
    assert_eq!(h.status(&x).await, Status::Done);
}

// ── scenario 29: approval thresholds as standing judgment ─────────────

fn baseline_config() -> ControllerConfig {
    ControllerConfig {
        approval: baseline(),
        ..ControllerConfig::default()
    }
}

#[tokio::test]
async fn a_named_side_effect_is_held_and_answering_its_question_releases_it() {
    let h = Harness::with(baseline_config(), StaticCatalog::default(), &["pinch"]);
    let mut invite = draft("m", "Invite the friends");
    invite.writable_resources = vec!["message:third_party".to_string()];
    let mut headcount = draft("n", "Confirm the headcount");
    headcount.edges = vec![on(EdgeKind::Blocks, "m")];
    let accepted = h
        .file(plan(vec![
            draft("P", "Plan the birthday"),
            draft("a", "Pick the restaurant"),
            invite,
            headcount,
        ]))
        .await;
    let (p, a, m, n) = (
        &accepted.ids["P"],
        &accepted.ids["a"],
        &accepted.ids["m"],
        &accepted.ids["n"],
    );
    assert_eq!(accepted.held, vec![m.clone(), n.clone()]);
    let outbox = h.outbox().await;
    assert!(outbox[0].body.contains("Needs your approval"));
    assert!(outbox[0].body.contains("Invite the friends"));
    assert!(outbox[0].body.contains(&format!("/approve {p}")));
    let approval = questions_of(&h, p).await;
    assert_eq!(approval.len(), 1);
    assert_eq!(approval[0].kind, QuestionKind::Approval);
    h.drain().await;
    assert_eq!(h.status(a).await, Status::Done, "the unheld sibling ran");
    assert_eq!(
        h.status(m).await,
        Status::Blocked(BlockedReason::NeedsConsent)
    );
    let reply = ControlHandle::answer(&h.ctl, &approval[0].id, "approve", "user")
        .await
        .unwrap();
    assert_eq!(reply.released.len(), 2);
    assert_eq!(reply.question.status, QuestionStatus::Answered);
    h.drain().await;
    assert_eq!(h.status(n).await, Status::Done);
}

#[tokio::test]
async fn a_plan_under_the_thresholds_runs_at_once_with_the_decision_recorded() {
    let h = Harness::with(baseline_config(), StaticCatalog::default(), &["pinch"]);
    let accepted = h
        .file(plan(vec![
            draft("P", "Plan a quiet dinner"),
            draft("a", "Pick the restaurant"),
        ]))
        .await;
    assert!(accepted.held.is_empty());
    let p = &accepted.ids["P"];
    let decision = h
        .events(p)
        .await
        .into_iter()
        .find(|e| e.kind == EventKind::Decision && e.actor == "policy")
        .expect("the delegated decision");
    let record: rustykrab_core::questions::DelegatedDecision =
        serde_json::from_str(decision.reason.as_deref().unwrap()).unwrap();
    assert!(
        record.authority.contains("baseline"),
        "{}",
        record.authority
    );
    assert!(
        questions_of(&h, p).await.is_empty(),
        "the user was not asked"
    );
    // A grant that lowers the item threshold holds the same shape of plan.
    ControlHandle::grant_judgment(&h.ctl, "Ask me about plans with more than 1 item.", "", "u")
        .await
        .unwrap();
    let accepted = h
        .file(plan(vec![
            draft("P", "Plan a loud dinner"),
            draft("a", "Pick the club"),
        ]))
        .await;
    assert_eq!(accepted.held.len(), 2, "held whole: {:?}", accepted.held);
}

// ── the planner (scenario 18) ──────────────────────────────────────────

/// A graph a planner stand-in files for a brief.
type Graph = Box<dyn Fn(&Brief) -> WorkPlan + Send + Sync>;

/// A planner stand-in: files the graphs it is given, in turn, through the
/// controller as its planning run, then ends like the real one (its output
/// is the accepted plan, which the controller turns into its report).
struct Planner {
    ctl: OnceLock<std::sync::Weak<Controller>>,
    graphs: Mutex<Vec<Graph>>,
    outcomes: Mutex<Vec<PlanOutcome>>,
}

#[async_trait]
impl Worker for Planner {
    fn name(&self) -> &str {
        "planner"
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::Local
    }
    fn planning_only(&self) -> bool {
        true
    }
    fn accepts(&self, item: &WorkItem) -> bool {
        // This stand-in implements graph filing, not general execution.
        crate::graph::is_planning(item)
    }
    fn capabilities(&self) -> WorkerCapabilities {
        WorkerCapabilities {
            models: vec!["planner-model".to_string()],
            tools: vec!["work_plan".to_string(), "work_status".to_string()],
            ..WorkerCapabilities::default()
        }
    }
    async fn run(&self, brief: Brief) -> Result<ResultReport, Error> {
        let ctl = self.ctl.get().and_then(|w| w.upgrade()).expect("bound");
        let graphs: Vec<WorkPlan> = self
            .graphs
            .lock()
            .unwrap()
            .iter()
            .map(|g| g(&brief))
            .collect();
        let binding = rustykrab_tools::work_backend::WorkRunContext {
            item: brief.item.clone(),
            actor: "worker:planner".to_string(),
        };
        for graph in graphs {
            let provenance = Provenance {
                conversation_id: None,
                filed_by_item: Some(brief.item.clone()),
                actor: "worker:planner".to_string(),
            };
            let outcome = WORK_RUN_CONTEXT
                .scope(binding.clone(), WorkBackend::plan(&*ctl, graph, provenance))
                .await?;
            let accepted = matches!(outcome, PlanOutcome::Accepted(_));
            self.outcomes.lock().unwrap().push(outcome);
            if accepted {
                break;
            }
        }
        Err(Error::Internal(
            "the planner's run ends on its accepted plan".into(),
        ))
    }
}

struct PlannerHarness {
    ctl: Arc<Controller>,
    clock: Arc<ManualClock>,
    script: Arc<Script>,
    planner: Arc<Planner>,
    _dir: TempDir,
}

impl PlannerHarness {
    fn new(config: ControllerConfig) -> PlannerHarness {
        let path = std::env::temp_dir().join(format!("rk-planner-{}", Uuid::new_v4()));
        let store = Store::open(&path, vec![9u8; 32]).expect("store opens");
        let clock = Arc::new(ManualClock::new(Utc::now()));
        let script = Arc::new(Script::default());
        let planner = Arc::new(Planner {
            ctl: OnceLock::new(),
            graphs: Mutex::new(Vec::new()),
            outcomes: Mutex::new(Vec::new()),
        });
        let pinch: Arc<dyn Worker> = Arc::new(Scripted {
            name: "pinch".to_string(),
            concurrency: 1,
            script: script.clone(),
        });
        let ctl = Arc::new(
            Controller::new(store, vec![pinch, planner.clone()], config).with_clock(clock.clone()),
        );
        let _ = planner.ctl.set(Arc::downgrade(&ctl));
        PlannerHarness {
            ctl,
            clock,
            script,
            planner,
            _dir: TempDir(path),
        }
    }

    async fn drain(&self) {
        for _ in 0..60 {
            self.ctl.wait_for_runs(Duration::from_millis(300)).await;
            self.clock.advance(TimeDelta::seconds(1));
            let r = ControlHandle::tick(&*self.ctl).await.expect("tick");
            let quiet = r.leased.is_empty() && r.reconciled.is_empty() && r.transitions == 0;
            if quiet && self.ctl.state().runs.is_empty() && self.ctl.state().finished.is_empty() {
                break;
            }
        }
    }

    async fn request(&self, title: &str) -> String {
        let mut d = draft("req", title);
        d.plan = true;
        let outcome = WorkBackend::file(
            &*self.ctl,
            d,
            Provenance {
                conversation_id: Some("conv-1".to_string()),
                filed_by_item: None,
                actor: "agent".to_string(),
            },
        )
        .await
        .unwrap();
        match outcome {
            PlanOutcome::Accepted(a) => a.root,
            PlanOutcome::Rejected(r) => panic!("rejected: {r:?}"),
        }
    }

    async fn item(&self, id: &str) -> WorkItem {
        self.ctl.store().work_get(id).await.unwrap().unwrap()
    }

    async fn children(&self, id: &str) -> Vec<WorkItem> {
        self.ctl.store().work_children(id).await.unwrap()
    }

    async fn events(&self, id: &str) -> Vec<WorkEvent> {
        self.ctl.store().work_events(id).await.unwrap()
    }
}

fn trip(root: &str, bad: bool) -> WorkPlan {
    let mut flights = draft("flights", "Book the flights");
    flights.parent = Some(tmp("trip"));
    let mut hotel = draft("hotel", "Book the hotel");
    hotel.parent = Some(tmp(if bad { "flights" } else { "trip" }));
    let mut calendar = draft("calendar", "Add the trip to the calendar");
    calendar.parent = Some(tmp("trip"));
    if bad {
        flights.edges = vec![on(EdgeKind::Blocks, "hotel")];
    } else {
        calendar.edges = vec![
            on(EdgeKind::Blocks, "flights"),
            on(EdgeKind::Blocks, "hotel"),
        ];
    }
    let mut items = vec![draft("trip", root), flights, hotel];
    if !bad {
        items.push(calendar);
    }
    WorkPlan {
        root: tmp("trip"),
        items,
        edges: Vec::new(),
        rationale: "the trip".to_string(),
    }
}

#[tokio::test]
async fn a_planned_request_is_planned_whole_and_nothing_runs_before_its_planning_item() {
    let h = PlannerHarness::new(ControllerConfig::default());
    h.planner
        .graphs
        .lock()
        .unwrap()
        .push(Box::new(|_| trip("Lisbon trip", true)));
    h.planner
        .graphs
        .lock()
        .unwrap()
        .push(Box::new(|_| trip("Lisbon trip", false)));
    let request = h.request("Plan the Lisbon trip").await;
    let kids = h.children(&request).await;
    assert_eq!(
        kids.len(),
        2,
        "one planning item and the request as one item"
    );
    let planning = kids
        .iter()
        .find(|k| crate::graph::is_planning(k))
        .expect("a planning child")
        .clone();
    let whole = kids
        .iter()
        .find(|k| !crate::graph::is_planning(k))
        .unwrap()
        .clone();
    h.drain().await;

    let outcomes = h.planner.outcomes.lock().unwrap().clone();
    assert_eq!(outcomes.len(), 2);
    let PlanOutcome::Rejected(first) = &outcomes[0] else {
        panic!("the first graph was accepted");
    };
    let cycle = first
        .failed
        .iter()
        .find(|f| f.reason == rustykrab_core::work::RejectionReason::Cycle)
        .expect("rejected with cycle");
    for t in ["flights", "hotel"] {
        assert!(cycle.offending.contains(&tmp(t)), "{:?}", cycle.offending);
    }
    let PlanOutcome::Accepted(second) = &outcomes[1] else {
        panic!("the corrected graph was rejected");
    };
    assert_eq!(second.ids.len(), 4);
    let trip_root = h.item(&second.ids["trip"]).await;
    assert_eq!(
        trip_root.parent.as_deref(),
        Some(request.as_str()),
        "under the request"
    );
    assert!(
        h.ctl
            .store()
            .work_edges_of(&trip_root.id)
            .await
            .unwrap()
            .iter()
            .any(|e| e.kind == EdgeKind::Blocks && e.depends_on == planning.id),
        "the graph waits on its planning item"
    );
    // Planning done before anything of the graph was leased.
    let planned_at = h
        .events(&planning.id)
        .await
        .into_iter()
        .find(|e| e.to == Some(Status::Done))
        .expect("the planning item is done")
        .at;
    for id in second.ids.values() {
        for e in h.events(id).await {
            if e.kind == EventKind::Lease {
                assert!(
                    e.at >= planned_at,
                    "{id} leased before its plan was reconciled"
                );
            }
        }
    }
    assert_eq!(
        h.item(&whole.id).await.status,
        Status::Cancelled(CancelReason::Cascade),
        "the request as one item was never needed"
    );
    assert_eq!(h.item(&request).await.status, Status::Done);
    // The rejection is an event dreaming can count, on the planning item.
    assert!(h
        .events(&planning.id)
        .await
        .iter()
        .any(|e| e.kind == EventKind::Rejection));
}

#[tokio::test]
async fn a_planning_run_that_never_files_a_graph_runs_the_request_as_one_item() {
    let h = PlannerHarness::new(ControllerConfig::default());
    let request = h.request("Plan the picnic").await;
    h.drain().await;
    let kids = h.children(&request).await;
    let planning = kids.iter().find(|k| crate::graph::is_planning(k)).unwrap();
    let whole = kids.iter().find(|k| !crate::graph::is_planning(k)).unwrap();
    assert_eq!(planning.status, Status::Failed);
    assert_eq!(
        whole.status,
        Status::Done,
        "the plan B ran the request whole"
    );
    assert_eq!(h.item(&request).await.status, Status::Done);
    assert!(h.script.briefs_for("Plan the picnic").len() == 1);
}

#[tokio::test]
async fn a_failed_chain_re_plans_at_the_parent_once_then_surfaces_once() {
    let h = PlannerHarness::new(ControllerConfig::default());
    for _ in 0..8 {
        h.script.push(
            "Order the groceries",
            report(failure(ErrorSubclass::Timeout, "upstream timed out")),
        );
    }
    let mut b = draft("b", "Order the groceries");
    b.edges = vec![on(EdgeKind::Blocks, "a")];
    let mut c = draft("c", "Cook the dinner");
    c.edges = vec![on(EdgeKind::Blocks, "b")];
    let outcome = ControlHandle::file_plan(
        &*h.ctl,
        plan(vec![
            draft("P", "Host the dinner"),
            draft("a", "Choose the menu"),
            b,
            c,
        ]),
        Provenance::default(),
        FilingSource::Planner,
    )
    .await
    .unwrap();
    let PlanOutcome::Accepted(accepted) = outcome else {
        panic!("rejected");
    };
    let (p, b, c) = (&accepted.ids["P"], &accepted.ids["b"], &accepted.ids["c"]);
    h.drain().await;
    assert_eq!(h.item(b).await.status, Status::Failed);
    assert_eq!(
        h.item(c).await.status,
        Status::Blocked(BlockedReason::UpstreamFailed)
    );
    let replans: Vec<WorkItem> = h
        .children(p)
        .await
        .into_iter()
        .filter(crate::graph::is_planning)
        .collect();
    assert_eq!(replans.len(), 1, "one re-plan per parent");
    assert!(replans[0].title.starts_with("Re-plan:"));
    assert_eq!(replans[0].status, Status::Failed, "it filed no graph");
    let parent = h.item(p).await;
    assert_eq!(
        parent.status,
        Status::Blocked(BlockedReason::UpstreamFailed)
    );
    assert_eq!(parent.status_origin.as_deref(), Some(b.as_str()));
    let rungs: Vec<Rung> = h
        .events(p)
        .await
        .iter()
        .filter_map(super::super::load::decode_rung)
        .map(|r| r.rung)
        .collect();
    assert_eq!(rungs, vec![Rung::Replan, Rung::Surface]);
    let outbox = h.ctl.store().work_outbox_pending().await.unwrap();
    let about: Vec<_> = outbox.iter().filter(|o| o.parent == *p).collect();
    assert_eq!(about.len(), 1, "surfaced once, at the parent: {about:#?}");
    assert!(about[0].body.contains("Order the groceries"));
    assert!(about[0].body.contains("Cook the dinner"));
}

// ── scenario 5 and the subtree stall ──────────────────────────────────

#[test]
fn a_stall_repairs_with_the_first_attempts_evidence() {
    // The worker half is `LocalWorker`'s ledger; the controller half is the
    // ladder: a loop failure repairs, and the repair brief carries the
    // first attempt's evidence (tested in `scenarios.rs` for scenario 13's
    // shape). Here the stall detail reaches the rung.
    use crate::errors::{classify, Context, FailureInput, ProviderProblem};
    use crate::progress::StepLedger;
    let mut ledger = StepLedger::new(2);
    for _ in 0..3 {
        ledger.record("todo_read", &serde_json::json!({}), "[]");
    }
    let stall = ledger.stalled().unwrap();
    let error = classify(
        &FailureInput::Provider {
            problem: ProviderProblem::Loop,
            detail: stall.detail(),
        },
        &Context::default(),
    );
    assert_eq!(error.subclass, ErrorSubclass::Loop);
    let state = crate::ladder::LadderState::default();
    let decision = crate::ladder::next(&state, &crate::ladder::LadderContext::new(&error));
    assert!(matches!(decision, crate::ladder::Decision::Repair { .. }));
}

#[tokio::test]
async fn a_repair_after_a_stall_carries_the_first_attempts_evidence() {
    let h = Harness::new(&["pinch"]);
    h.script.push(
        "Tidy the downloads",
        Step::Fail(
            crate::worker::RunFailure::Model {
                problem: crate::errors::ProviderProblem::Loop,
                detail: "stalled: 6 steps without new evidence (progress ledger)".to_string(),
            }
            .into_error(),
        ),
    );
    let x = h.file_one(draft("x", "Tidy the downloads")).await;
    h.drain().await;
    assert_eq!(h.status(&x).await, Status::Done);
    let events = h.events(&x).await;
    let repair = events
        .iter()
        .filter_map(super::super::load::decode_rung)
        .find(|r| r.rung == Rung::Repair)
        .expect("the stall triggered repair");
    let error = repair.error.unwrap();
    assert_eq!(error.subclass, ErrorSubclass::Loop, "the stall is typed");
    assert!(error.detail.contains("progress ledger"));
    let briefs = h.script.briefs_for("Tidy the downloads");
    assert_eq!(briefs.len(), 2);
    assert!(
        briefs[1].prior_evidence.iter().any(|e| e.kind == "run"),
        "the second attempt carries the first attempt's run: {:?}",
        briefs[1].prior_evidence
    );
    assert!(briefs[1]
        .last_error
        .as_deref()
        .unwrap_or("")
        .contains("stalled"));
}

// ── notices (6.6) ──────────────────────────────────────────────────────

#[tokio::test]
async fn notices_coalesce_within_the_window_and_a_question_goes_at_once() {
    let config = ControllerConfig {
        coalesce_window: TimeDelta::seconds(30),
        ..ControllerConfig::default()
    };
    let h = Harness::with(config, StaticCatalog::default(), &["pinch"]);
    let x = h.file_one(draft("x", "Water the plants")).await;
    h.drain().await;
    assert_eq!(h.status(&x).await, Status::Done);
    assert!(
        h.outbox().await.is_empty(),
        "a report waits out the coalescing window"
    );
    let waiting = h
        .store()
        .work_outbox_waiting(&x, "default", chrono::Utc::now())
        .await
        .unwrap()
        .expect("the report is waiting");
    assert!(waiting.body.contains("Water the plants"));

    h.script.push(
        "Pick a colour",
        report(asking("Blue or green?", "blocking_now", &["blue", "green"])),
    );
    let y = h.file_one(draft("y", "Pick a colour")).await;
    h.drain().await;
    let due = h.outbox().await;
    assert_eq!(due.len(), 1, "the question went at once");
    assert_eq!(due[0].parent, y);
}

#[tokio::test]
async fn a_chain_open_past_the_digest_window_gets_a_digest() {
    let config = ControllerConfig {
        digest_window: TimeDelta::seconds(5),
        ..ControllerConfig::default()
    };
    let h = Harness::with(config, StaticCatalog::default(), &["pinch"]);
    let mut later = draft("q", "Book the venue");
    later.trigger = rustykrab_core::work::Trigger::At(h.clock.now() + TimeDelta::days(3));
    let accepted = h
        .file(plan(vec![draft("P", "Plan the reunion"), later]))
        .await;
    let p = &accepted.ids["P"];
    for _ in 0..3 {
        h.tick().await;
    }
    assert!(h.outbox().await.is_empty());
    for _ in 0..8 {
        h.tick().await;
    }
    let digests: Vec<_> = h
        .outbox()
        .await
        .into_iter()
        .filter(|o| o.parent == *p && o.body.contains("digest"))
        .collect();
    assert!(!digests.is_empty(), "no digest for an open chain");
}

// ── on_answer ──────────────────────────────────────────────────────────

#[tokio::test]
async fn an_on_answer_trigger_fires_when_its_question_is_answered() {
    let h = Harness::new(&["pinch"]);
    h.script.push(
        "Choose a date",
        report(asking("Which date?", "blocking_now", &["3 May", "4 May"])),
    );
    let x = h.file_one(draft("x", "Choose a date")).await;
    h.drain().await;
    let q = questions_of(&h, &x).await.remove(0);
    let mut follow = draft("f", "Book the table");
    follow.trigger = rustykrab_core::work::Trigger::OnAnswer(q.id.clone());
    let f = h.file_one(follow).await;
    h.drain().await;
    assert_eq!(h.status(&f).await, Status::Queued, "waits on the answer");
    ControlHandle::answer(&h.ctl, &q.id, "1", "user")
        .await
        .unwrap();
    h.drain().await;
    assert_eq!(h.status(&f).await, Status::Done);
    assert_eq!(h.status(&x).await, Status::Done);
}

#[tokio::test]
async fn an_unresolved_question_is_reused_before_or_after_a_batch_commit() {
    use crate::controller::{batch::Batch, questions::NewQuestion};
    let h = Harness::new(&[]);
    let item = h.file_one(draft("repeat", "Choose the direction")).await;
    let make = || {
        NewQuestion::open(
            &item,
            QuestionKind::Decision,
            "Choose A or B?".into(),
            "blocking_now",
        )
    };
    let mut b = Batch::new(h.ctl.load().await.unwrap(), h.clock.now());
    let first = h.ctl.record_question(&mut b, make()).await.unwrap();
    assert_eq!(h.ctl.record_question(&mut b, make()).await.unwrap(), first);
    h.ctl
        .commit(b, &mut std::collections::HashSet::new())
        .await
        .unwrap();
    let mut b = Batch::new(h.ctl.load().await.unwrap(), h.clock.now());
    assert_eq!(h.ctl.record_question(&mut b, make()).await.unwrap(), first);
    assert!(
        b.ops.is_empty(),
        "a repeated unresolved ask must not create a record or notification"
    );
    assert_eq!(questions_of(&h, &item).await.len(), 1);
}

#[tokio::test]
async fn historical_repeats_consolidate_without_deciding_and_old_links_still_answer() {
    use rustykrab_store::QuestionWrite;
    let h = Harness::new(&["pinch"]);
    h.script.push(
        "Choose direction",
        report(asking("Pick A or B?", "blocking_now", &["A", "B"])),
    );
    let item = h.file_one(draft("direction", "Choose direction")).await;
    h.drain().await;
    let original = questions_of(&h, &item).await[0].clone();
    let mut duplicate = original.clone();
    duplicate.id = uuid::Uuid::new_v4().to_string();
    duplicate.created_at += chrono::Duration::seconds(1);
    h.store()
        .question_write(QuestionWrite::Insert(Box::new(duplicate.clone())))
        .await
        .unwrap();
    let mut distinct = original.clone();
    distinct.id = uuid::Uuid::new_v4().to_string();
    distinct.options = vec!["A".into(), "C".into()];
    distinct.created_at += chrono::Duration::seconds(2);
    h.store()
        .question_write(QuestionWrite::Insert(Box::new(distinct.clone())))
        .await
        .unwrap();
    ControlHandle::tick(&h.ctl).await.unwrap();
    let d = h
        .store()
        .question_get(&duplicate.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(d.superseded_by.as_deref(), Some(original.id.as_str()));
    assert_eq!(d.status, QuestionStatus::Obsolete);
    assert!(d.answer.is_none());
    assert_eq!(
        h.status(&item).await,
        Status::Blocked(BlockedReason::NeedsDecision)
    );
    let reply = ControlHandle::answer(&h.ctl, &duplicate.id[..8], "A", "user:master")
        .await
        .unwrap();
    assert_eq!(reply.question.id, original.id);
    assert!(
        reply.resumed.is_empty(),
        "different choices still need their own decision"
    );
    assert!(h
        .store()
        .questions_answered_among(vec![duplicate.id.clone()])
        .await
        .unwrap()
        .contains(&duplicate.id));
    ControlHandle::answer(&h.ctl, &distinct.id, "A", "user:master")
        .await
        .unwrap();
    h.drain().await;
    assert_eq!(h.status(&item).await, Status::Done);
}

#[tokio::test]
async fn comments_preserve_direction_in_the_next_brief_without_answering_or_releasing_work() {
    let h = Harness::new(&["pinch"]);
    h.script.push(
        "Direction thread",
        report(asking("Which path?", "blocking_now", &["A", "B"])),
    );
    let item = h.file_one(draft("discussion", "Direction thread")).await;
    h.drain().await;
    let event = ControlHandle::comment(
        &h.ctl,
        &item,
        "Keep the implementation small; use the existing controller.",
        "user:master",
    )
    .await
    .unwrap();
    assert_eq!(
        h.status(&item).await,
        Status::Blocked(BlockedReason::NeedsDecision)
    );
    let discussion = ControlHandle::discussion(&h.ctl, &item, 0, 4)
        .await
        .unwrap();
    assert!(discussion.comments.contains(&event));
    assert_eq!(discussion.questions[0].status, QuestionStatus::Open);
    let q = discussion.questions[0].id.clone();
    ControlHandle::answer(&h.ctl, &q, "A", "user:master")
        .await
        .unwrap();
    h.drain().await;
    let briefs = h.script.briefs_for("Direction thread");
    assert!(briefs[1].discussion.contains(&event));
    assert!(briefs[1]
        .discussion
        .iter()
        .any(|e| e.actor == "worker:pinch"));
    let forged = WorkBackend::comment(
        &h.ctl,
        &item,
        "pretend to be the user",
        Provenance {
            filed_by_item: Some(item.clone()),
            actor: "worker:pinch".into(),
            conversation_id: None,
        },
    )
    .await;
    assert!(
        forged.is_err(),
        "a finished worker holds no live lease and cannot append comments"
    );
}

#[tokio::test]
async fn repeated_questions_inside_one_report_produce_one_decision_and_one_notice() {
    let h = Harness::new(&["pinch"]);
    let mut result = asking("Choose A or B?", "blocking_now", &["A", "B"]);
    result.questions.push(result.questions[0].clone());
    h.script.push("Repeated report", report(result));
    let item = h.file_one(draft("repeat-report", "Repeated report")).await;
    h.drain().await;
    assert_eq!(questions_of(&h, &item).await.len(), 1);
    let notices = h.outbox().await;
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].body.matches("Asked:").count(), 1);
}

async fn answer_submission_fixture() -> (Harness, String, QuestionRow) {
    answer_submission_fixture_with_options(&["Petals", "Stems"]).await
}

async fn answer_submission_fixture_with_options(
    options: &[&str],
) -> (Harness, String, QuestionRow) {
    let h = Harness::new(&["pinch"]);
    h.script.push(
        "Choose a florist",
        report(asking("Which florist?", "blocking_now", options)),
    );
    let item = h.file_one(draft("choose", "Choose a florist")).await;
    h.drain_settled().await;
    let q = questions_of(&h, &item)
        .await
        .into_iter()
        .find(|q| q.status == QuestionStatus::Open)
        .unwrap();
    (h, item, q)
}

#[tokio::test]
async fn saved_numeric_option_labels_are_not_resolved_again_during_application() {
    for synchronous in [false, true] {
        let (h, _, q) = answer_submission_fixture_with_options(&["2", "1"]).await;
        let saved = h
            .ctl
            .submit_answer(&q.id, "1", "user:master", None)
            .await
            .unwrap();
        assert_eq!(saved.question.answer_submission.unwrap().answer, "2");
        if synchronous {
            h.ctl.answer(&q.id, "1", "user:master").await.unwrap();
        } else {
            h.ctl.tick().await.unwrap();
        }
        let applied = h.store().question_get(&q.id).await.unwrap().unwrap();
        assert_eq!(applied.answer.as_deref(), Some("2"));
        assert_eq!(
            applied.answer_submission.unwrap().status,
            rustykrab_store::AnswerSubmissionStatus::Applied
        );
    }
}

#[tokio::test]
async fn answer_submission_is_prompt_while_the_controller_lock_is_held_and_resumes_real_work() {
    use rustykrab_store::AnswerSubmissionStatus;
    let (h, item, q) = answer_submission_fixture().await;
    let busy = h.ctl.loop_lock.lock().await;
    let id = uuid::Uuid::new_v4().to_string();
    let saved = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        h.ctl.submit_answer(&q.id, "2", "user:master", Some(&id)),
    )
    .await
    .expect("accepting an answer must not wait for the controller pass")
    .unwrap();
    assert_eq!(
        saved.question.answer_submission.as_ref().unwrap().status,
        AnswerSubmissionStatus::Pending
    );
    assert_eq!(saved.question.answer, None);
    assert_eq!(
        h.status(&item).await,
        Status::Blocked(BlockedReason::NeedsDecision)
    );
    let retry = h
        .ctl
        .submit_answer(&q.id, "2", "user:master", Some(&id))
        .await
        .unwrap();
    assert_eq!(
        saved, retry,
        "a retry returns the original receipt and timestamp"
    );
    assert!(h
        .ctl
        .submit_answer(&q.id, "1", "user:master", Some(&id))
        .await
        .is_err());
    assert!(h
        .ctl
        .submit_answer(&q.id, "1", "user:master", None)
        .await
        .is_err());
    drop(busy);
    h.drain_settled().await;
    let applied = h.store().question_get(&q.id).await.unwrap().unwrap();
    assert_eq!(
        applied.answer_submission.as_ref().unwrap().status,
        AnswerSubmissionStatus::Applied
    );
    assert_eq!(applied.answer.as_deref(), Some("Stems"));
    assert_eq!(h.status(&item).await, Status::Done);
    assert!(h
        .script
        .briefs_for("Choose a florist")
        .last()
        .unwrap()
        .decisions_made
        .iter()
        .any(|d| d.contains("Stems")));
    let retry = h
        .ctl
        .submit_answer(&q.id, "2", "user:master", Some(&id))
        .await
        .unwrap();
    assert_eq!(
        retry.question, applied,
        "a lost applied acknowledgement can also be reconciled"
    );
}

#[tokio::test]
async fn a_saved_answer_survives_controller_replacement_and_one_failed_tick() {
    use rustykrab_store::AnswerSubmissionStatus;
    let (h, _, q) = answer_submission_fixture().await;
    h.ctl
        .submit_answer(&q.id, "1", "user:master", None)
        .await
        .unwrap();
    let restored = Controller::new(h.store().clone(), vec![], ControllerConfig::default())
        .with_clock(h.clock.clone());
    restored.state().fail_next_tick = Some(Error::Storage("temporary fixture failure".into()));
    assert!(restored.tick().await.is_err());
    let pending = h.store().question_get(&q.id).await.unwrap().unwrap();
    assert_eq!(
        pending.answer_submission.unwrap().status,
        AnswerSubmissionStatus::Pending
    );
    restored.tick().await.unwrap();
    let applied = h.store().question_get(&q.id).await.unwrap().unwrap();
    assert_eq!(applied.answer.as_deref(), Some("Petals"));
    assert_eq!(
        applied.answer_submission.unwrap().status,
        AnswerSubmissionStatus::Applied
    );
}

#[tokio::test]
async fn saved_answer_for_work_cancelled_before_application_is_rejected_visibly() {
    use rustykrab_store::AnswerSubmissionStatus;
    let (h, item, q) = answer_submission_fixture().await;
    h.ctl
        .submit_answer(&q.id, "1", "user:master", None)
        .await
        .unwrap();
    h.ctl
        .cancel(&item, Some("owner cancelled".into()), "user:master")
        .await
        .unwrap();
    h.ctl.tick().await.unwrap();
    let closed = h.store().question_get(&q.id).await.unwrap().unwrap();
    let receipt = closed.answer_submission.unwrap();
    assert_eq!(receipt.status, AnswerSubmissionStatus::Rejected);
    assert!(receipt.error.unwrap().contains("closed"));
    assert_eq!(closed.answer, None);
    assert!(matches!(h.status(&item).await, Status::Cancelled(_)));
}

#[tokio::test]
async fn superseding_a_question_does_not_redirect_accepted_owner_direction() {
    use rustykrab_store::{AnswerSubmissionStatus, QuestionWrite};
    let (h, item, q) = answer_submission_fixture().await;
    let saved = h
        .ctl
        .submit_answer(&q.id, "1", "user:master", None)
        .await
        .unwrap();
    let submission = saved.question.answer_submission.unwrap().id;
    let mut replacement = q.clone();
    replacement.id = uuid::Uuid::new_v4().to_string();
    replacement.text = "A different decision now needs review".into();
    h.store()
        .question_write(QuestionWrite::Insert(Box::new(replacement.clone())))
        .await
        .unwrap();
    h.store()
        .question_write(QuestionWrite::Supersede {
            id: q.id.clone(),
            canonical: replacement.id.clone(),
            at: h.clock.now(),
        })
        .await
        .unwrap();
    let retry = h
        .ctl
        .submit_answer(&q.id[..8], "1", "user:master", Some(&submission))
        .await
        .unwrap();
    assert_eq!(
        retry.question.id, q.id,
        "a retry must not target the replacement question"
    );
    h.ctl.tick().await.unwrap();
    let old = h.store().question_get(&q.id).await.unwrap().unwrap();
    assert_eq!(
        old.answer_submission.unwrap().status,
        AnswerSubmissionStatus::Rejected
    );
    let retry = h
        .ctl
        .submit_answer(&q.id, "1", "user:master", Some(&submission))
        .await
        .unwrap();
    assert_eq!(retry.question.id, q.id);
    assert_eq!(
        retry.question.answer_submission.unwrap().status,
        AnswerSubmissionStatus::Rejected
    );
    assert_eq!(
        h.store()
            .question_get(&replacement.id)
            .await
            .unwrap()
            .unwrap()
            .answer,
        None
    );
    assert_eq!(
        h.status(&item).await,
        Status::Blocked(BlockedReason::NeedsDecision)
    );
}

#[tokio::test]
async fn malformed_consent_is_rejected_before_receipt_acceptance() {
    use rustykrab_store::QuestionWrite;
    let (h, _, mut q) = answer_submission_fixture().await;
    q.id = uuid::Uuid::new_v4().to_string();
    q.kind = QuestionKind::Consent;
    q.options.clear();
    h.store()
        .question_write(QuestionWrite::Insert(Box::new(q.clone())))
        .await
        .unwrap();
    assert!(h
        .ctl
        .submit_answer(&q.id, "perhaps", "user:master", None)
        .await
        .is_err());
    assert_eq!(
        h.store()
            .question_get(&q.id)
            .await
            .unwrap()
            .unwrap()
            .answer_submission,
        None
    );
}

#[tokio::test]
async fn saved_approval_and_rejection_complete_with_their_plan_effects() {
    use rustykrab_store::AnswerSubmissionStatus;
    for answer in ["approve", "reject"] {
        let h = Harness::with(baseline_config(), StaticCatalog::default(), &["pinch"]);
        let mut invite = draft("m", "Invite the friends");
        invite.writable_resources = vec!["message:third_party".into()];
        let accepted = h
            .file(plan(vec![draft("P", "Plan the birthday"), invite]))
            .await;
        let root = &accepted.ids["P"];
        let item = &accepted.ids["m"];
        let q = questions_of(&h, root).await.remove(0);
        assert_eq!(q.kind, QuestionKind::Approval);
        let id = uuid::Uuid::new_v4().to_string();
        h.ctl
            .submit_answer(&q.id, answer, "user:master", Some(&id))
            .await
            .unwrap();
        assert!(h
            .store()
            .work_get(item)
            .await
            .unwrap()
            .unwrap()
            .held_by
            .is_some());
        {
            let _guard = h.ctl.loop_lock.lock().await;
            h.ctl.apply_pending_answers_locked().await.unwrap();
        }
        let settled = h.store().question_get(&q.id).await.unwrap().unwrap();
        assert_eq!(settled.status, QuestionStatus::Answered);
        assert_eq!(
            settled.answer_submission.as_ref().unwrap().status,
            AnswerSubmissionStatus::Applied
        );
        if answer == "approve" {
            assert!(h
                .store()
                .work_get(item)
                .await
                .unwrap()
                .unwrap()
                .held_by
                .is_none());
        } else {
            assert!(matches!(h.status(item).await, Status::Cancelled(_)));
        }
        let retry = h
            .ctl
            .submit_answer(&q.id, answer, "user:master", Some(&id))
            .await
            .unwrap();
        assert_eq!(retry.question, settled);
        assert!(retry.released.is_empty());
        assert!(retry.cancelled.is_empty());
    }
}
