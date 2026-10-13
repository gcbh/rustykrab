//! Blocked states that wait on the host (plan section 7): a credential
//! question on the credential page, credentials stored by any path, and an
//! item no healthy worker can take.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use chrono::TimeDelta;
use rustykrab_core::questions::{QuestionKind, QuestionStatus};
use rustykrab_core::work::{BlockedReason, Trigger, WorkKind};
use rustykrab_store::credential_backend::MemoryBackend;
use rustykrab_store::{QuestionFilter, QuestionRow};
use rustykrab_tools::work_backend::ToolState;

use super::*;
use crate::controller::{CredentialLinks, StoredCredentials, ToolCatalog};

/// A catalog whose credentials are the registry's, as the daemon's is.
struct RegistryBacked(Arc<StoredCredentials>);

impl ToolCatalog for RegistryBacked {
    fn tool_state(&self, _name: &str) -> ToolState {
        ToolState::Unknown
    }

    fn mcp_server_configured(&self, _name: &str) -> bool {
        false
    }

    fn credential_available(&self, name: &str) -> bool {
        self.0.available(name)
    }
}

/// A worker whose health the test sets.
struct Toggle {
    inner: Scripted,
    kind: WorkerKind,
    up: Arc<AtomicBool>,
}

#[async_trait]
impl Worker for Toggle {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn kind(&self) -> WorkerKind {
        self.kind
    }

    fn capabilities(&self) -> WorkerCapabilities {
        let mut caps = self.inner.capabilities();
        if self.kind == WorkerKind::Peer {
            caps.tools.push("work_plan".into());
        }
        caps
    }

    fn healthy(&self) -> bool {
        self.up.load(Ordering::SeqCst)
    }

    async fn run(&self, brief: Brief) -> Result<ResultReport, Error> {
        self.inner.run(brief).await
    }
}

/// A harness over a store whose credentials live in memory (never the
/// keychain), with the fleet, catalog and credential links given.
fn harness_with(
    fleet: impl FnOnce(&Arc<Script>) -> Vec<Arc<dyn Worker>>,
    catalog: Arc<dyn ToolCatalog>,
    links: Option<CredentialLinks>,
) -> Harness {
    let path = std::env::temp_dir().join(format!("rk-controller-{}", Uuid::new_v4()));
    let store = Store::open(&path, vec![9u8; 32])
        .expect("store opens")
        .with_credential_backend(Arc::new(MemoryBackend::new()));
    let clock = Arc::new(ManualClock::new(Utc::now()));
    let script = Arc::new(Script::default());
    let config = ControllerConfig::default();
    let mut ctl = Controller::new(store.clone(), fleet(&script), config.clone())
        .with_clock(clock.clone())
        .with_catalog(catalog);
    if let Some(links) = links {
        ctl = ctl.with_credential_links(links);
    }
    Harness {
        ctl,
        clock,
        script,
        store,
        config,
        catalog: Arc::new(StaticCatalog::default()),
        dir: Arc::new(TempDir(path)),
    }
}

fn pinch(script: &Arc<Script>) -> Scripted {
    Scripted {
        name: "pinch".to_string(),
        concurrency: 1,
        script: Arc::clone(script),
    }
}

fn credential_harness(links: Option<CredentialLinks>, stored: Arc<StoredCredentials>) -> Harness {
    harness_with(
        |script| vec![Arc::new(pinch(script)) as Arc<dyn Worker>],
        Arc::new(RegistryBacked(stored)),
        links,
    )
}

/// One worker of `kind` whose health `up` says.
fn toggle_harness(kind: WorkerKind, up: Arc<AtomicBool>) -> Harness {
    harness_with(
        |script| {
            vec![Arc::new(Toggle {
                inner: pinch(script),
                kind,
                up,
            }) as Arc<dyn Worker>]
        },
        Arc::new(StaticCatalog::default()),
        None,
    )
}

async fn questions_of(h: &Harness, item: &str) -> Vec<QuestionRow> {
    h.store()
        .questions_list(&QuestionFilter {
            item: Some(item.to_string()),
            ..QuestionFilter::default()
        })
        .await
        .unwrap()
}

const BASE: &str = "https://krab.test";

// ── needs_credential on the credential page ────────────────────────────

#[tokio::test]
async fn a_credential_question_sends_the_credential_page_out_of_band_and_fulfilment_resumes_it() {
    let links = CredentialLinks::with_base(Some(BASE.to_string()));
    let stored = Arc::new(StoredCredentials::new());
    let h = credential_harness(Some(links.clone()), Arc::clone(&stored));
    h.script.push(
        "Pay the invoice",
        report(failure(
            ErrorSubclass::Credential,
            "needs credential: bank login",
        )),
    );
    let x = h.file_one(draft("x", "Pay the invoice")).await;
    h.drain().await;
    assert_eq!(
        h.status(&x).await,
        Status::Blocked(BlockedReason::NeedsCredential)
    );

    // The request is filed the way `credential_request` files one, under
    // the credential's store key, and nothing else asks for it.
    let requests = h.store().credential_requests();
    let pending = requests.pending().await.unwrap();
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert_eq!(pending[0].name, "bank_login");
    assert_eq!(pending[0].service.as_deref(), Some("bank login"));
    assert_eq!(pending[0].fields.len(), 1);
    assert_eq!(pending[0].fields[0].key, "bank_login");
    assert!(pending[0].fields[0].secret);

    // The notice says a link follows; the link is in no row the store keeps.
    let outbox = h.outbox().await;
    assert_eq!(outbox.len(), 1, "{outbox:#?}");
    let body = outbox[0].body.clone();
    assert!(body.contains("a one-time link follows"), "{body}");
    assert!(!body.contains(BASE), "{body}");
    let asked = questions_of(&h, &x).await;
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].kind, QuestionKind::Credential);
    assert!(!asked[0].text.contains(BASE));
    for event in h.events(&x).await {
        assert!(!format!("{event:?}").contains(BASE), "{event:?}");
    }

    // The channel takes it for the notice, once; it opens the request.
    assert_eq!(links.waiting(), 1);
    let sent = links.take_for_notice(&body);
    assert_eq!(sent.len(), 1);
    let token = sent[0]
        .strip_prefix(&format!("{BASE}/c/"))
        .expect("a credential page link");
    let opened = requests.find_by_link(token).await.unwrap();
    assert_eq!(opened.map(|r| r.id), Some(pending[0].id.clone()));
    assert!(links.take_for_notice(&body).is_empty(), "sent once");

    // Nothing moves until the credential is stored.
    h.drain().await;
    assert_eq!(
        h.status(&x).await,
        Status::Blocked(BlockedReason::NeedsCredential)
    );

    // The user fills in the page: the credential lands, the item resumes.
    requests
        .fulfil(
            &pending[0].id,
            &[("bank_login".to_string(), "hunter2".to_string())],
            "phone",
        )
        .await
        .unwrap();
    stored.refresh(&h.store().secrets()).await.unwrap();
    h.drain().await;
    assert_eq!(h.status(&x).await, Status::Done);
    let asked = questions_of(&h, &x).await;
    assert_eq!(asked[0].status, QuestionStatus::Answered);
    assert!(asked[0]
        .answer
        .as_deref()
        .is_some_and(|a| a.contains("is stored")));
    let caps = h.of_kind(WorkKind::Capability).await;
    assert_eq!(caps.len(), 1);
    assert_eq!(caps[0].status, Status::Done);
}

#[tokio::test]
async fn a_second_ask_for_the_same_credential_keeps_the_link_already_sent() {
    let links = CredentialLinks::with_base(Some(BASE.to_string()));
    let h = credential_harness(Some(links.clone()), Arc::new(StoredCredentials::new()));
    for title in ["Pay the invoice", "Check the balance"] {
        h.script.push(
            title,
            report(failure(
                ErrorSubclass::Credential,
                "needs credential: bank login",
            )),
        );
    }
    let ids = h
        .file(plan(vec![
            draft("P", "Bank errands"),
            draft("a", "Pay the invoice"),
            draft("b", "Check the balance"),
        ]))
        .await
        .ids;
    h.drain().await;
    for id in [&ids["a"], &ids["b"]] {
        assert_eq!(
            h.status(id).await,
            Status::Blocked(BlockedReason::NeedsCredential)
        );
    }
    let pending = h.store().credential_requests().pending().await.unwrap();
    assert_eq!(pending.len(), 1, "one request, not superseded: {pending:?}");
    let outbox = h.outbox().await;
    let bodies: Vec<&str> = outbox.iter().map(|r| r.body.as_str()).collect();
    assert!(
        bodies
            .iter()
            .any(|b| b.contains("with the link already sent for it")),
        "{bodies:#?}"
    );
    let sent: Vec<String> = bodies
        .iter()
        .flat_map(|b| links.take_for_notice(b))
        .collect();
    assert_eq!(sent.len(), 1, "one link for one credential");
}

#[tokio::test]
async fn without_credential_links_a_credential_question_files_no_request() {
    let h = credential_harness(None, Arc::new(StoredCredentials::new()));
    h.script.push(
        "Pay the invoice",
        report(failure(
            ErrorSubclass::Credential,
            "needs credential: bank login",
        )),
    );
    h.file_one(draft("x", "Pay the invoice")).await;
    h.drain().await;
    assert!(h
        .store()
        .credential_requests()
        .pending()
        .await
        .unwrap()
        .is_empty());
    let outbox = h.outbox().await;
    assert!(
        outbox[0]
            .body
            .contains("Store the credential, then /answer"),
        "{}",
        outbox[0].body
    );
}

// ── credentials stored by any path ─────────────────────────────────────

#[tokio::test]
async fn an_on_credential_trigger_fires_for_a_credential_stored_by_any_path() {
    let stored = Arc::new(StoredCredentials::new());
    let h = credential_harness(None, Arc::clone(&stored));
    let mut d = draft("x", "Download the statement");
    d.trigger = Trigger::OnCredential("dentist portal".to_string());
    let x = h.file_one(d).await;
    h.drain().await;
    assert_eq!(h.status(&x).await, Status::Queued);

    // Stored without any request (the CLI, `credential_write`): a registry
    // row under the credential's store key.
    h.store()
        .secrets()
        .create("dentist_portal", "pw")
        .await
        .unwrap();
    assert!(!stored.available("dentist portal"), "not refreshed yet");
    stored.refresh(&h.store().secrets()).await.unwrap();
    assert!(stored.available("dentist portal"));
    assert!(stored.available("dentist_portal"));
    h.drain().await;
    assert_eq!(h.status(&x).await, Status::Done);
}

// ── availability stays queued ─────────────────────────────────────────

#[tokio::test]
async fn unavailable_work_waits_without_timeout_budget_spend_or_questions() {
    let up = Arc::new(AtomicBool::new(false));
    let h = toggle_harness(WorkerKind::Local, Arc::clone(&up));
    let x = h.file_one(draft("x", "Sort the photos")).await;
    h.tick().await;
    assert_eq!(h.status(&x).await, Status::Ready);
    h.clock.advance(TimeDelta::days(45));
    h.tick().await;
    assert_eq!(h.status(&x).await, Status::Ready);
    assert!(h.rungs(&x).await.is_empty());
    assert!(questions_of(&h, &x).await.is_empty());
    assert!(h.outbox().await.is_empty());
    assert_eq!(h.ctl.spent_of(&x), rustykrab_store::Spend::default());
    assert!(h.script.briefs().is_empty());

    up.store(true, Ordering::SeqCst);
    h.drain().await;
    let completed = h
        .store()
        .work_get(&x)
        .await
        .unwrap()
        .map(|item| item.status)
        .or(h
            .store()
            .work_archive_get(&x)
            .await
            .unwrap()
            .map(|item| item.status));
    assert_eq!(completed, Some(Status::Done));
    assert!(h.rungs(&x).await.is_empty());
}

#[tokio::test]
async fn a_busy_worker_keeps_other_work_queued() {
    let h = Harness::new(&["pinch"]);
    let gate = Arc::new(Notify::new());
    h.script.push(
        "Long one",
        Step::Wait(Arc::clone(&gate), Box::new(done("Long one"))),
    );
    let a = h.file_one(draft("a", "Long one")).await;
    let b = h.file_one(draft("b", "Short one")).await;
    h.tick().await;
    h.tick().await;
    assert_eq!(h.status(&a).await, Status::Running);
    assert_eq!(h.status(&b).await, Status::Ready);
    gate.notify_one();
    h.drain().await;
    assert_eq!(h.status(&b).await, Status::Done);
    assert!(h.rungs(&b).await.is_empty());
}

#[tokio::test]
async fn waiting_does_not_fail_a_parent_or_release_its_plan_b() {
    let up = Arc::new(AtomicBool::new(false));
    let h = toggle_harness(WorkerKind::Local, Arc::clone(&up));
    let a = draft("a", "Refactor the parser");
    let mut b = draft("b", "Alternative implementation");
    b.edges = vec![on(EdgeKind::ConditionalOnFailure, "a")];
    let ids = h
        .file(plan(vec![draft("P", "Parser work"), a, b]))
        .await
        .ids;
    h.tick().await;
    h.clock.advance(TimeDelta::days(2));
    h.tick().await;
    assert_eq!(h.status(&ids["a"]).await, Status::Ready);
    assert_eq!(h.status(&ids["b"]).await, Status::Queued);
    assert!(!h.status(&ids["P"]).await.is_closed());
    assert!(h.rungs(&ids["a"]).await.is_empty());
    assert!(h.rungs(&ids["P"]).await.is_empty());
    assert!(questions_of(&h, &ids["a"]).await.is_empty());

    up.store(true, Ordering::SeqCst);
    let tick = h.tick().await;
    assert!(tick.leased.contains(&ids["a"]));
    assert_eq!(h.status(&ids["a"]).await, Status::Running);
    assert_eq!(h.status(&ids["b"]).await, Status::Queued);
}

#[tokio::test]
async fn legacy_backend_pins_do_not_prevent_an_available_executor() {
    let h = toggle_harness(WorkerKind::Local, Arc::new(AtomicBool::new(true)));
    let mut d = draft("x", "Review the result");
    d.worker_kind = WorkerKind::Codex;
    let x = h.file_one(d).await;
    h.drain().await;
    assert_eq!(h.status(&x).await, Status::Done);
    assert_eq!(h.item(&x).await.worker_kind, WorkerKind::Any);
    assert!(!h.script.briefs().is_empty());
}

#[tokio::test]
async fn an_outage_after_dispatch_returns_to_the_queue_without_spending_retries() {
    let h = toggle_harness(WorkerKind::Local, Arc::new(AtomicBool::new(true)));
    h.script.push(
        "Wait for provider",
        Step::Fail(Error::ModelRateLimit("quota exhausted".into())),
    );
    let x = h.file_one(draft("x", "Wait for provider")).await;
    h.tick().await;
    tokio::task::yield_now().await;
    h.tick().await;
    assert_eq!(h.status(&x).await, Status::Ready);
    assert!(h.rungs(&x).await.is_empty());
    assert!(questions_of(&h, &x).await.is_empty());
    assert_eq!(h.ctl.spent_of(&x), rustykrab_store::Spend::default());
    assert_eq!(h.script.briefs_for("Wait for provider").len(), 1);

    h.clock.advance(TimeDelta::seconds(31));
    h.drain().await;
    assert_eq!(h.status(&x).await, Status::Done);
    assert!(h.rungs(&x).await.is_empty());
}

#[tokio::test]
async fn legacy_availability_holds_are_requeued_even_before_capacity_returns() {
    use crate::controller::batch::Batch;
    use crate::controller::questions::NewQuestion;
    let h = toggle_harness(WorkerKind::Local, Arc::new(AtomicBool::new(false)));
    let x = h.file_one(draft("x", "Previously parked work")).await;
    let mut b = Batch::new(h.ctl.load().await.unwrap(), h.clock.now());
    b.move_to(
        &x,
        Status::Blocked(BlockedReason::WorkerUnavailable),
        "controller",
        "legacy availability hold",
    );
    h.ctl
        .record_question(
            &mut b,
            NewQuestion::open(
                &x,
                QuestionKind::Decision,
                "Needs capacity: a worker".into(),
                "surfaced",
            ),
        )
        .await
        .unwrap();
    b.settle(vec![x.clone()]);
    h.ctl
        .commit(b, &mut std::collections::HashSet::new())
        .await
        .unwrap();

    let mut error = failure(ErrorSubclass::Compute, "needs capacity: a worker")
        .error
        .unwrap();
    error.observed_by = "capability_check".into();
    h.store()
        .work_event_append(&WorkEvent {
            item: x.clone(),
            at: h.clock.now(),
            kind: EventKind::Rung,
            from: None,
            to: None,
            actor: "controller".into(),
            reason: Some(crate::ladder::encode_rung_event(
                &rustykrab_core::work::RungEvent {
                    rung: Rung::Request,
                    at: h.clock.now(),
                    error: Some(error),
                    outcome: "waiting for a worker".into(),
                },
            )),
            upstream: None,
            origin: None,
            evidence_ref: None,
        })
        .await
        .unwrap();

    h.tick().await;
    assert_eq!(h.status(&x).await, Status::Ready);
    assert!(
        h.rungs(&x).await.is_empty(),
        "old availability charges are retired"
    );
    let questions = questions_of(&h, &x).await;
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0].status, QuestionStatus::Obsolete);
    assert!(
        h.events(&x)
            .await
            .iter()
            .any(|event| event.kind == EventKind::Rung),
        "audit history remains"
    );
}

#[tokio::test]
async fn peer_advertising_plan_tool_still_runs_ordinary_work() {
    let h = toggle_harness(WorkerKind::Peer, Arc::new(AtomicBool::new(true)));
    assert!(
        !h.ctl.has_planner(),
        "a node advertisement is not a planning role"
    );
    let mut d = draft("peer", "Peer ordinary task");
    d.worker_kind = WorkerKind::Peer;
    let id = h.file_one(d).await;
    h.drain().await;
    let row = h.item(&id).await;
    assert_eq!(row.status, Status::Done);
    assert!(!h.script.briefs().is_empty());
}
