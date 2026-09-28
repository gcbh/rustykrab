//! `PeerWorker`: the `peer` worker kind of the control layer (plan
//! `docs/plans/control-layer-and-worker-fleet.md`, section 5, Phase 5).
//!
//! A peer is a paired RustyKrab node on the tailnet. Its worker runs a
//! brief there over the node's delegated-task API
//! (`rustykrab_control::peer` is the contract):
//!
//! - **submit** `POST /api/tasks` with the brief as typed fields, the
//!   tools to activate before the node's first model call, the work item
//!   and the controller's run id, and the brief rendered as text for a node
//!   that only runs text. The run id makes the submission idempotent: a
//!   resubmission gets back the task the node already has. A required tool
//!   outside the node's ceiling comes back `422` and ends the run with a
//!   typed failure (a tool the node lacks is a capability gap, one its
//!   policy withholds a `policy/ceiling` stop), never a run without it.
//! - **poll** `GET /api/tasks/{id}` until the task ends. The node may
//!   restart meanwhile: while it does not answer the worker marks itself
//!   unhealthy and keeps polling, and the node puts an interrupted task
//!   back in its queue, so the run goes on. A task the node no longer has
//!   is submitted again.
//! - **the result** is the node's typed `ResultReport` (its
//!   `result_json`), for a done task and for a failed one the node could
//!   type. A done task with only text (a node without structured
//!   delegation) is a `model/format` failure; a failed or cancelled one
//!   without a type is a process failure.
//! - **a timeout**: the smaller of the worker's and the brief's wall
//!   budget. When it passes the node's task is cancelled and the run ends
//!   `budget/wall`, or with a network error when the node never answered.
//! - **cancel** is propagated: [`Worker::stop`] (the controller stopped the
//!   run) sends `DELETE /api/tasks/{id}`. Dropping the run's future alone,
//!   as a controller that shuts down does, cancels nothing, so a restarted
//!   controller can re-attach ([`Worker::resumable`] asks the node for the
//!   task by run id).
//! - **usage** is what the node's worker counted (tokens, iterations,
//!   completion reminders) with the wall time the controller waited.
//! - **advertisement**: [`Worker::refresh`] reads `GET /api/node` (models,
//!   the tools and MCP servers inside the node's delegation ceiling,
//!   machine, writable resources), at most every `refresh_every` while the
//!   node answers and on every refresh while it does not. The worker is
//!   healthy only while its node answers and takes structured briefs, and
//!   advertises what the node last said.
//!
//! The worker authenticates with its token in the `Authorization` header
//! and sends a loopback `Origin`, as the `nodes` tool does. Nothing in a
//! submission is a credential (plan section 12.1): the brief's typed fields
//! only, and never this machine's worktree.

use std::collections::HashMap;
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rustykrab_control::errors::{BudgetKind, GapKind, PolicyStop, ProviderProblem};
use rustykrab_control::peer::{
    CeilingRefused, NodeAdvertisement, RefusalReason, TaskSubmission, TaskView, NODE_PATH,
    OUTSIDE_CEILING, TASKS_PATH,
};
use rustykrab_control::registry::WorkerSpec;
use rustykrab_control::worker::{Brief, RunFailure, RunUsage, Worker, WorkerCapabilities};
use rustykrab_core::work::{ResultReport, WorkerKind};
use rustykrab_core::{Error, Result, ToolError};
use serde::{Deserialize, Serialize};

use crate::local_worker::render_brief;

/// The `Origin` node-to-node requests carry: a loopback origin every node
/// accepts. The node's origin check guards against browsers; the token is
/// the authentication.
const ORIGIN: &str = "http://127.0.0.1:3000";
/// A run's longest wait when neither the spec nor the brief says less.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3_600);
const POLL: Duration = Duration::from_secs(2);
const REFRESH_EVERY: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Characters of a node's text quoted in a failure.
const QUOTE_MAX: usize = 200;

/// How a [`PeerWorker`] reaches its node.
#[derive(Debug, Clone)]
pub struct PeerConfig {
    /// The node's gateway, without a trailing `/`.
    pub base_url: String,
    pub token: String,
    /// The longest one run may take; the brief's wall budget may make it
    /// shorter.
    pub timeout: Duration,
    /// Between polls of a running task.
    pub poll: Duration,
    pub concurrency: usize,
    /// How often the advertisement is re-read while the node answers.
    pub refresh_every: Duration,
}

impl PeerConfig {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> PeerConfig {
        PeerConfig {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
            timeout: DEFAULT_TIMEOUT,
            poll: POLL,
            concurrency: 1,
            refresh_every: REFRESH_EVERY,
        }
    }

    /// The config a registry spec describes: its `base_url`, `token`,
    /// `timeout_seconds` and `concurrency`.
    pub fn from_spec(spec: &WorkerSpec) -> std::result::Result<PeerConfig, String> {
        let base = spec
            .base_url
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .ok_or("a peer needs its node's base_url")?;
        let token = spec
            .token
            .as_deref()
            .filter(|t| !t.trim().is_empty())
            .ok_or("no token is stored for this peer")?;
        let mut config = PeerConfig::new(base, token);
        if let Some(secs) = spec.timeout_seconds.filter(|s| *s > 0) {
            config.timeout = Duration::from_secs(secs);
        }
        if let Some(n) = spec.concurrency.filter(|n| *n > 0) {
            config.concurrency = n;
        }
        Ok(config)
    }
}

/// What the node last said about itself.
#[derive(Debug, Clone, Default)]
struct Standing {
    advert: NodeAdvertisement,
    /// The node answered the last request.
    answered: bool,
    /// Why it is not usable, when it is not.
    why: String,
    /// When the advertisement was last read.
    read_at: Option<Instant>,
}

/// The `peer` worker kind: briefs run on a paired node.
pub struct PeerWorker {
    name: String,
    config: PeerConfig,
    client: reqwest::Client,
    standing: RwLock<Standing>,
    /// Run id to the node's task id, while the run is live.
    tasks: Mutex<HashMap<String, String>>,
    /// What each ended run spent, until the controller asks.
    spent: Mutex<HashMap<String, RunUsage>>,
}

/// What one request to the node came to.
enum Reply<T> {
    Ok(T),
    /// A status the caller handles, with the body.
    Status(u16, String),
    /// No answer: the node is down, restarting or unreachable.
    Unreachable(String),
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl PeerWorker {
    pub fn new(name: impl Into<String>, config: PeerConfig) -> PeerWorker {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .expect("failed to build HTTP client");
        PeerWorker {
            name: name.into(),
            config,
            client,
            standing: RwLock::new(Standing {
                why: "not asked yet".to_string(),
                ..Standing::default()
            }),
            tasks: Mutex::new(HashMap::new()),
            spent: Mutex::new(HashMap::new()),
        }
    }

    /// Why the worker is not usable, or `None` when it is.
    pub fn unavailable(&self) -> Option<String> {
        let s = self.standing.read().unwrap_or_else(|e| e.into_inner());
        (!(s.answered && s.advert.structured)).then(|| s.why.clone())
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.config.base_url)
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, self.url(path))
            .bearer_auth(&self.config.token)
            .header("Origin", ORIGIN)
    }

    /// Send a request and read a JSON body of type `T` from a success.
    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Reply<T> {
        let response = match request.send().await {
            Ok(r) => r,
            Err(e) => return Reply::Unreachable(format!("{} is unreachable: {e}", self.name)),
        };
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        if !(200..300).contains(&status) {
            // A node restarting behind a proxy answers 502 or 503: that is
            // not an answer from the node.
            if matches!(status, 502..=504) {
                return Reply::Unreachable(format!("{} answered {status}", self.name));
            }
            return Reply::Status(status, body);
        }
        match serde_json::from_str(&body) {
            Ok(value) => Reply::Ok(value),
            Err(e) => Reply::Status(status, format!("unreadable reply ({e}): {}", quote(&body))),
        }
    }

    fn heard(&self) {
        let mut s = self.standing.write().unwrap_or_else(|e| e.into_inner());
        s.answered = true;
        if s.advert.structured {
            s.why.clear();
        }
    }

    fn not_heard(&self, why: &str) {
        let mut s = self.standing.write().unwrap_or_else(|e| e.into_inner());
        s.answered = false;
        s.why = why.to_string();
    }

    /// The submission for one run: typed fields, and the brief as text. A
    /// peer never gets this machine's worktree, and may not delegate
    /// onward.
    fn submission(&self, mut brief: Brief, run: &str) -> TaskSubmission {
        brief.workspace = None;
        brief.run = Some(run.to_string());
        TaskSubmission {
            message: render_brief(&brief),
            conversation_id: None,
            hop_budget: Some(0),
            allowed_tools: None,
            trace_id: Some(run.to_string()),
            work_item_id: Some(brief.item.clone()),
            required_tools: brief.required_tools.clone(),
            run: Some(run.to_string()),
            brief: Some(brief),
        }
    }

    /// Submit, retrying while the node does not answer, until `deadline`.
    async fn submit(&self, body: &TaskSubmission, deadline: Instant) -> Result<TaskView> {
        loop {
            let request = self.request(reqwest::Method::POST, TASKS_PATH).json(body);
            match self.call::<TaskView>(request).await {
                Reply::Ok(view) => {
                    self.heard();
                    if let Some(run) = &body.run {
                        lock(&self.tasks).insert(run.clone(), view.id.clone());
                    }
                    return Ok(view);
                }
                Reply::Status(422, text) => {
                    self.heard();
                    return Err(refused(&self.name, &text));
                }
                Reply::Status(status @ (401 | 403), _) => {
                    let why = format!(
                        "{}'s node refused this worker's token ({status})",
                        self.name
                    );
                    self.not_heard(&why);
                    return Err(Error::ToolExecution(ToolError::permission_denied(why)));
                }
                Reply::Status(status, text) => {
                    self.heard();
                    return Err(Error::ToolExecution(ToolError::invalid_input(format!(
                        "{}'s node rejected the submission ({status}): {}",
                        self.name,
                        quote(&text)
                    ))));
                }
                Reply::Unreachable(why) => {
                    self.not_heard(&why);
                    if Instant::now() >= deadline {
                        return Err(Error::ToolExecution(ToolError::transient(format!(
                            "{why}; the brief was never submitted"
                        ))));
                    }
                    tokio::time::sleep(self.config.poll).await;
                }
            }
        }
    }

    /// Cancel a task on the node, best effort.
    async fn cancel_task(&self, task: &str) {
        let request = self.request(reqwest::Method::DELETE, &format!("{TASKS_PATH}/{task}"));
        if let Reply::Unreachable(why) | Reply::Status(_, why) =
            self.call::<TaskView>(request).await
        {
            tracing::warn!(worker = %self.name, task, %why, "the node's task was not cancelled");
        }
    }

    fn keep_usage(&self, run: &str, view: &TaskView, started: Instant) {
        let waited = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let node = view.usage.unwrap_or_default();
        let usage = RunUsage {
            wall_ms: waited.max(node.wall_ms),
            ..node
        };
        let mut spent = lock(&self.spent);
        if spent.len() >= 64 {
            spent.clear();
        }
        spent.insert(run.to_string(), usage);
    }
}

#[async_trait]
impl Worker for PeerWorker {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> WorkerKind {
        WorkerKind::Peer
    }

    /// What the node last advertised: models, the tools and MCP servers
    /// inside its delegation ceiling, its machine and the resources a
    /// delegated run may write there. Empty until it first answers.
    fn capabilities(&self) -> WorkerCapabilities {
        self.standing
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .advert
            .capabilities
            .clone()
    }

    fn concurrency(&self) -> usize {
        self.config.concurrency
    }

    /// While the node answers and takes structured briefs.
    fn healthy(&self) -> bool {
        self.unavailable().is_none()
    }

    fn usage(&self, run: &str) -> Option<RunUsage> {
        lock(&self.spent).remove(run)
    }

    async fn run(&self, brief: Brief) -> Result<ResultReport> {
        let started = Instant::now();
        let run = brief
            .run
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let limit = match brief.budget.wall_seconds {
            0 => self.config.timeout,
            secs => self.config.timeout.min(Duration::from_secs(secs)),
        };
        let deadline = started + limit;
        let body = self.submission(brief, &run);
        let mut task = self.submit(&body, deadline).await?;
        let mut unreachable: Option<String> = None;
        let view = loop {
            if task.is_terminal() {
                break task;
            }
            let now = Instant::now();
            if now >= deadline {
                lock(&self.tasks).remove(&run);
                if let Some(why) = unreachable {
                    return Err(Error::ToolExecution(ToolError::transient(format!(
                        "{why}; no result within {}s",
                        limit.as_secs()
                    ))));
                }
                self.cancel_task(&task.id).await;
                return Err(RunFailure::Budget {
                    budget: BudgetKind::Wall,
                    detail: format!(
                        "{}s on {} without a result; the node's task was cancelled",
                        limit.as_secs(),
                        self.name
                    ),
                }
                .into_error());
            }
            tokio::time::sleep(self.config.poll.min(deadline - now)).await;
            let request = self.request(reqwest::Method::GET, &format!("{TASKS_PATH}/{}", task.id));
            match self.call::<TaskView>(request).await {
                Reply::Ok(view) => {
                    self.heard();
                    unreachable = None;
                    if view.attempts > task.attempts && task.attempts > 0 {
                        tracing::info!(
                            worker = %self.name, %run, attempts = view.attempts,
                            "the node restarted mid-run and runs the task again"
                        );
                    }
                    task = view;
                }
                // The node no longer has the task (its store was replaced,
                // or it swept it): the brief goes again, under the same run.
                Reply::Status(404, _) => {
                    self.heard();
                    tracing::warn!(worker = %self.name, %run, "the node lost the task; submitting it again");
                    task = self.submit(&body, deadline).await?;
                }
                Reply::Status(status @ (401 | 403), _) => {
                    let why = format!(
                        "{}'s node refused this worker's token ({status})",
                        self.name
                    );
                    self.not_heard(&why);
                    lock(&self.tasks).remove(&run);
                    return Err(Error::ToolExecution(ToolError::permission_denied(why)));
                }
                Reply::Status(status, text) => {
                    // An answer, but not the task: keep polling to the
                    // deadline rather than fail a run that may be fine.
                    tracing::warn!(worker = %self.name, status, text = %quote(&text), "unexpected reply while polling");
                }
                Reply::Unreachable(why) => {
                    self.not_heard(&why);
                    unreachable = Some(why);
                }
            }
        };
        lock(&self.tasks).remove(&run);
        self.keep_usage(&run, &view, started);
        outcome(&self.name, view)
    }

    /// Asks the node for the task this run submitted. One it has, and has
    /// not cancelled, is this worker's to finish; while the node does not
    /// answer the run is kept too, since a resubmission under the same run
    /// id is the task the node has, or a fresh one if it never got it.
    async fn resumable(&self, run: &str) -> bool {
        let request = self
            .request(reqwest::Method::GET, TASKS_PATH)
            .query(&[("run", run)]);
        match self.call::<Vec<TaskView>>(request).await {
            Reply::Ok(views) => {
                self.heard();
                match views.into_iter().find(|v| v.run.as_deref() == Some(run)) {
                    Some(view) => {
                        let keep = view.status != "cancelled";
                        if keep {
                            lock(&self.tasks).insert(run.to_string(), view.id);
                        }
                        keep
                    }
                    None => false,
                }
            }
            Reply::Status(status, text) => {
                tracing::warn!(worker = %self.name, status, text = %quote(&text), "could not look the run up on the node");
                false
            }
            Reply::Unreachable(why) => {
                self.not_heard(&why);
                true
            }
        }
    }

    /// Cancel the run's task on the node, in the background.
    fn stop(&self, run: &str) {
        let Some(task) = lock(&self.tasks).remove(run) else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let request = self.request(reqwest::Method::DELETE, &format!("{TASKS_PATH}/{task}"));
        let name = self.name.clone();
        runtime.spawn(async move {
            match request.send().await {
                Ok(r) if r.status().is_success() || r.status().as_u16() == 404 => {
                    tracing::info!(worker = %name, %task, "cancelled the node's task");
                }
                Ok(r) => tracing::warn!(worker = %name, %task, status = %r.status(), "the node did not cancel its task"),
                Err(e) => tracing::warn!(worker = %name, %task, error = %e, "could not reach the node to cancel its task"),
            }
        });
    }

    /// Re-read the node's advertisement: every time while the node does not
    /// answer, otherwise at most every `refresh_every`.
    async fn refresh(&self) -> bool {
        {
            let s = self.standing.read().unwrap_or_else(|e| e.into_inner());
            let fresh = s
                .read_at
                .is_some_and(|t| t.elapsed() < self.config.refresh_every);
            if s.answered && fresh {
                return false;
            }
        }
        let reply = self
            .call::<NodeAdvertisement>(self.request(reqwest::Method::GET, NODE_PATH))
            .await;
        let mut s = self.standing.write().unwrap_or_else(|e| e.into_inner());
        match reply {
            Reply::Ok(advert) => {
                s.answered = true;
                s.read_at = Some(Instant::now());
                s.why = if advert.structured {
                    String::new()
                } else {
                    "the node does not take structured briefs".to_string()
                };
                s.advert = advert;
            }
            Reply::Status(status @ (404 | 405), _) => {
                s.answered = true;
                s.read_at = Some(Instant::now());
                s.advert.structured = false;
                s.why = format!(
                    "the node serves no advertisement ({status}): a build without structured \
                     delegation"
                );
            }
            Reply::Status(status, text) => {
                s.answered = status != 401 && status != 403;
                s.why = format!("the node answered {status}: {}", quote(&text));
            }
            Reply::Unreachable(why) => {
                s.answered = false;
                s.why = why;
            }
        }
        true
    }
}

/// A run's typed outcome from the node's final view of its task.
fn outcome(name: &str, view: TaskView) -> Result<ResultReport> {
    match (view.status.as_str(), view.report) {
        ("done", Some(report)) => Ok(report),
        ("done", None) => Err(RunFailure::Model {
            problem: ProviderProblem::Format,
            detail: format!(
                "{name} answered with text, not a result report: {}",
                quote(view.result.as_deref().unwrap_or(""))
            ),
        }
        .into_error()),
        ("failed", Some(report)) if report.error.is_some() => Ok(report),
        (status, _) => Err(RunFailure::Process {
            code: None,
            stderr_tail: format!(
                "{name}'s node ended the task {status}: {}",
                quote(view.error.as_deref().unwrap_or("no reason given"))
            ),
        }
        .into_error()),
    }
}

/// A `422` refusal as a typed failure: a tool the node lacks is a
/// capability gap; one its policy withholds is a `policy/ceiling` stop.
fn refused(name: &str, body: &str) -> Error {
    let Ok(refusal) = serde_json::from_str::<CeilingRefused>(body) else {
        return Error::ToolExecution(ToolError::invalid_input(format!(
            "{name}'s node refused the submission: {}",
            quote(body)
        )));
    };
    if refusal.error != OUTSIDE_CEILING {
        return Error::ToolExecution(ToolError::invalid_input(format!(
            "{name}'s node refused the submission: {}",
            refusal.message
        )));
    }
    if let Some(missing) = refusal
        .refused
        .iter()
        .find(|r| r.reason == RefusalReason::Unknown)
    {
        return RunFailure::Gap {
            gap: GapKind::Tool,
            name: missing.tool.clone(),
        }
        .into_error();
    }
    RunFailure::Policy {
        stop: PolicyStop::Ceiling,
        detail: format!("{name}: {}", refusal.message),
    }
    .into_error()
}

fn quote(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= QUOTE_MAX {
        return flat;
    }
    let mut cut: String = flat.chars().take(QUOTE_MAX - 3).collect();
    cut.push_str("...");
    cut
}

/// What `POST /api/pair` answers: the new device's id and token, and the
/// node's advertisement when it has one.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Paired {
    pub device_id: String,
    pub device_token: String,
    #[serde(default)]
    pub capabilities: Option<NodeAdvertisement>,
}

/// Redeem a pairing code at `base_url` for a device token named
/// `device_name` (plan section 5: a peer is paired, not given the node's
/// master token).
pub async fn redeem_pairing_code(
    base_url: &str,
    code: &str,
    device_name: &str,
) -> std::result::Result<Paired, String> {
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let url = format!("{}/api/pair", base_url.trim_end_matches('/'));
    let response = client
        .post(&url)
        .header("Origin", ORIGIN)
        .json(&serde_json::json!({ "code": code, "deviceName": device_name }))
        .send()
        .await
        .map_err(|e| format!("{url} is unreachable: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "the node refused the pairing code ({status}): it is wrong, used, or expired"
        ));
    }
    response
        .json::<Paired>()
        .await
        .map_err(|e| format!("unreadable pairing reply: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustykrab_control::errors::{classify, Context};
    use rustykrab_control::peer::Refusal;
    use rustykrab_control::worker::run_failure_input;
    use rustykrab_core::work::{Budget, ErrorClass, ErrorSubclass, WorkKind};

    fn view(status: &str) -> TaskView {
        TaskView {
            id: "t1".into(),
            status: status.into(),
            conversation_id: None,
            result: None,
            error: None,
            created_at: "2026-09-28T00:00:00Z".into(),
            started_at: None,
            finished_at: None,
            elapsed_secs: 0,
            work_item_id: None,
            required_tools: Vec::new(),
            run: None,
            attempts: 1,
            report: None,
            usage: None,
        }
    }

    fn class_of(err: &Error) -> (ErrorClass, ErrorSubclass) {
        let e = classify(
            &run_failure_input(err),
            &Context {
                tool: None,
                worker_kind: Some(WorkerKind::Peer),
            },
        );
        (e.class, e.subclass)
    }

    fn brief() -> Brief {
        Brief {
            item: "41".into(),
            kind: WorkKind::Personal,
            title: "Check the harbour calendar".into(),
            objective: "Check Saturday".into(),
            done_when: "Saturday is listed".into(),
            constraints: Vec::new(),
            decisions_made: Vec::new(),
            artifact_refs: Vec::new(),
            required_tools: vec!["caldav".into()],
            required_mcp_servers: Vec::new(),
            writable_resources: Vec::new(),
            inputs: Vec::new(),
            more_inputs: Vec::new(),
            prior_evidence: Vec::new(),
            last_error: None,
            budget: Budget::default(),
            origin_conversation_id: None,
            run: None,
            workspace: None,
            capability: None,
        }
    }

    #[test]
    fn a_submission_carries_the_brief_typed_and_as_text() {
        let worker = PeerWorker::new("krabby", PeerConfig::new("http://n/", "t"));
        let body = worker.submission(brief(), "run-1");
        assert_eq!(body.required_tools, ["caldav"]);
        assert_eq!(body.work_item_id.as_deref(), Some("41"));
        assert_eq!(body.run.as_deref(), Some("run-1"));
        assert_eq!(body.hop_budget, Some(0), "a peer never delegates onward");
        assert_eq!(body.brief.as_ref().unwrap().run.as_deref(), Some("run-1"));
        assert!(body.message.contains("Check the harbour calendar"));
        let wire = serde_json::to_string(&body).unwrap();
        assert!(
            !wire.contains("\"t\""),
            "the token travels in a header only"
        );
        assert_eq!(worker.url(TASKS_PATH), "http://n/api/tasks");
    }

    #[test]
    fn the_nodes_final_view_becomes_a_typed_outcome() {
        let mut done = view("done");
        done.report = Some(ResultReport {
            summary: "Checked.".into(),
            ..ResultReport::default()
        });
        assert_eq!(outcome("krabby", done).unwrap().summary, "Checked.");

        let mut text = view("done");
        text.result = Some("Saturday is free.".into());
        let err = outcome("krabby", text).unwrap_err();
        assert_eq!(class_of(&err), (ErrorClass::Model, ErrorSubclass::Format));

        let mut typed = view("failed");
        typed.report = Some(rustykrab_control::peer::failed_run_report(
            "krabby",
            &RunFailure::Budget {
                budget: BudgetKind::Tokens,
                detail: "spent".into(),
            }
            .into_error(),
        ));
        let report = outcome("krabby", typed).unwrap();
        assert_eq!(report.error.unwrap().subclass, ErrorSubclass::Tokens);

        let mut restarted = view("failed");
        restarted.error = Some("interrupted: the node restarted mid-task 3 times".into());
        let err = outcome("krabby", restarted).unwrap_err();
        assert_eq!(class_of(&err).0, ErrorClass::Environment);
    }

    #[test]
    fn a_ceiling_refusal_is_typed_by_why() {
        let body = |reason| {
            serde_json::to_string(&CeilingRefused::new(vec![Refusal {
                tool: "caldav".into(),
                reason,
            }]))
            .unwrap()
        };
        let gap = refused("krabby", &body(RefusalReason::Unknown));
        assert_eq!(
            class_of(&gap),
            (ErrorClass::CapabilityGap, ErrorSubclass::ToolGap)
        );
        let policy = refused("krabby", &body(RefusalReason::NodePolicy));
        assert_eq!(
            class_of(&policy),
            (ErrorClass::Policy, ErrorSubclass::Ceiling)
        );
    }

    #[test]
    fn a_spec_without_a_token_is_not_a_peer() {
        let mut spec = WorkerSpec {
            kind: WorkerKind::Peer,
            base_url: Some("http://127.0.0.1:3100/".into()),
            ..WorkerSpec::default()
        };
        assert!(PeerConfig::from_spec(&spec).is_err());
        spec.token = Some("t".into());
        spec.timeout_seconds = Some(90);
        let config = PeerConfig::from_spec(&spec).unwrap();
        assert_eq!(config.base_url, "http://127.0.0.1:3100");
        assert_eq!(config.timeout, Duration::from_secs(90));
    }

    #[tokio::test]
    async fn an_unreachable_node_is_unhealthy_and_its_runs_are_kept() {
        // Nothing listens on port 9 of loopback.
        let worker = PeerWorker::new("krabby", PeerConfig::new("http://127.0.0.1:9", "t"));
        assert!(!worker.healthy());
        assert!(worker.refresh().await);
        assert!(!worker.healthy());
        assert!(worker.unavailable().unwrap().contains("unreachable"));
        assert!(
            worker.resumable("run-1").await,
            "a node that is down may still hold the run"
        );
        assert!(worker.capabilities().tools.is_empty());
    }
}
