//! The control plan's Phase 6 wired into the daemon: the nightly
//! evaluation pass, `POST /api/work/evaluate`, and the GitHub issue
//! adapter of the review surface (`docs/plans/control-layer-and-worker-fleet.md`,
//! sections 1.1, 10 and 11).
//!
//! One pass is, in order: the review surface's decisions read back and
//! applied through the controller ([`rustykrab_control::review::pull_decisions`]),
//! dreaming's evaluation ([`rustykrab_dream::Evaluation`]: metrics, criteria,
//! proposals, probation), then every engineering item projected out
//! ([`rustykrab_control::review::push_projections`]). Passes never overlap.
//!
//! The adapters that bridge the pieces live here because only the binary
//! holds all of them: [`ControlFiler`] files dreaming's drafts through the
//! controller's validator, [`StoreRouting`] reads the worker registry's
//! routing records and class default tiers for dreaming's routing
//! criterion, and [`GithubIssues`] is the first [`ReviewSurface`], configured by `RUSTYKRAB_GITHUB_REPO` and
//! `RUSTYKRAB_GITHUB_API_BASE` with its token from the credential store
//! (`github_token`, overridable by `RUSTYKRAB_GITHUB_TOKEN`). Without a repo
//! and a token nothing is projected and the rest of the pass runs.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use serde_json::{json, Value};

use rustykrab_control::handle::ControlHandle;
use rustykrab_control::registry::{WorkerRegistry, WorkerView};
use rustykrab_control::review::{self, Comment, Issue, Projection, ReviewSurface, LABEL_MANAGED};
use rustykrab_core::proposal::{EvaluationReport, ProjectionReport};
use rustykrab_core::work::WorkerKind;
use rustykrab_core::work::{ArtifactRef, PlanOutcome, WorkError, WorkItemDraft};
use rustykrab_core::Error;
use rustykrab_dream::evaluate::RoutingEntry;
use rustykrab_dream::{
    Evaluation, EvaluationConfig, ProposalFiler, RoutingRecordReader, StoreLedger,
    StoreOutcomeSource, StoreQuestions, StoreWorkRecords,
};
use rustykrab_gateway::evaluate_routes::EvaluationHandle;
use rustykrab_store::Store;
use rustykrab_tools::work_backend::Provenance;

/// Files dreaming's drafts through the controller's validator, as
/// `dreaming`, so the scope limit and every other check apply to them.
pub struct ControlFiler {
    control: Arc<dyn ControlHandle>,
    actor: String,
}

impl ControlFiler {
    fn provenance(&self) -> Provenance {
        Provenance {
            conversation_id: None,
            filed_by_item: None,
            actor: self.actor.clone(),
        }
    }
}

#[async_trait]
impl ProposalFiler for ControlFiler {
    async fn file(&self, draft: WorkItemDraft) -> rustykrab_core::Result<PlanOutcome> {
        self.control.file_draft(draft, self.provenance()).await
    }

    async fn file_internal(
        &self,
        error: &WorkError,
        evidence: Vec<ArtifactRef>,
    ) -> rustykrab_core::Result<PlanOutcome> {
        let draft = rustykrab_control::errors::internal_item_draft(error, evidence);
        self.control.file_draft(draft, self.provenance()).await
    }
}

// ── the routing record ─────────────────────────────────────────────────

/// Dreaming's view of the worker registry (plan sections 5 and 10): one
/// entry per worker and class of work from its routing record, with the
/// worker's cost tier and whether it holds the class's default today.
///
/// A class's default tier is the store's `routing_defaults` row; the
/// worker holding it is the one a new item of the class goes to first: the
/// cheapest live, healthy worker at or above that tier, the earliest
/// registered on a tie. `cost` is the mean tokens a judged run of the class
/// took.
pub struct StoreRouting {
    registry: Arc<WorkerRegistry>,
}

impl StoreRouting {
    pub fn new(registry: Arc<WorkerRegistry>) -> Self {
        StoreRouting { registry }
    }
}

/// The entries for `workers` (in registration order) under `defaults`.
pub fn routing_entries(
    workers: &[WorkerView],
    defaults: &std::collections::BTreeMap<String, u32>,
) -> Vec<RoutingEntry> {
    let holder = |tier: u32| -> Option<&str> {
        workers
            .iter()
            .enumerate()
            .filter(|(_, w)| w.live && w.healthy && w.cost_tier >= tier)
            .min_by_key(|(i, w)| (w.cost_tier, *i))
            .map(|(_, w)| w.name.as_str())
    };
    let n = |v: u64| u32::try_from(v).unwrap_or(u32::MAX);
    let mut out = Vec::new();
    for w in workers {
        let kind = WorkerKind::parse(&w.kind).unwrap_or_default();
        for (class, r) in &w.routing_record {
            let default_tier = defaults.get(class).copied();
            out.push(RoutingEntry {
                worker: w.name.clone(),
                worker_kind: kind,
                cost_tier: w.cost_tier,
                class: class.clone(),
                verified_done: n(r.verified_done),
                claimed_not_verified: n(r.claimed_not_verified),
                escaped_defects: n(r.escaped_defects),
                review_rejections: 0,
                repairs: n(r.repairs),
                cost: if r.cost.runs == 0 {
                    0.0
                } else {
                    r.cost.tokens as f64 / r.cost.runs as f64
                },
                probation: r.probation,
                default_for_class: default_tier.and_then(holder).is_some_and(|h| h == w.name),
                default_tier,
                items: r.recent_items.clone(),
            });
        }
    }
    out
}

#[async_trait]
impl RoutingRecordReader for StoreRouting {
    async fn entries(&self) -> rustykrab_core::Result<Vec<RoutingEntry>> {
        let workers = self.registry.views().await?;
        let defaults = self
            .registry
            .store()
            .workers()
            .default_tiers()
            .await?
            .into_iter()
            .map(|d| (d.class, d.tier))
            .collect();
        Ok(routing_entries(&workers, &defaults))
    }
}

// ── the GitHub issue adapter ───────────────────────────────────────────

/// GitHub issues as the review surface: one repository, every projected
/// issue under the `rustykrab` label.
pub struct GithubIssues {
    client: reqwest::Client,
    api: String,
    repo: String,
    token: String,
}

/// How many pages of issues a listing reads at most (100 per page).
const MAX_PAGES: u32 = 50;

fn surface_error(what: &str, e: impl std::fmt::Display) -> Error {
    Error::Channel(format!("github {what}: {e}"))
}

fn labels_of(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|all| {
            all.iter()
                .filter_map(|l| {
                    l.as_str()
                        .or_else(|| l["name"].as_str())
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn issue_of(v: &Value) -> Option<Issue> {
    let number = match &v["number"] {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        _ => return None,
    };
    Some(Issue {
        number,
        url: v["html_url"].as_str().map(str::to_string),
        title: v["title"].as_str().unwrap_or_default().to_string(),
        body: v["body"].as_str().unwrap_or_default().to_string(),
        labels: labels_of(&v["labels"]),
        open: v["state"].as_str() != Some("closed"),
    })
}

impl GithubIssues {
    pub fn new(api: &str, repo: &str, token: &str) -> Result<Self, Error> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent(concat!("rustykrab/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| surface_error("client", e))?;
        Ok(GithubIssues {
            client,
            api: api.trim_end_matches('/').to_string(),
            repo: repo.to_string(),
            token: token.to_string(),
        })
    }

    fn url(&self, tail: &str) -> String {
        format!("{}/repos/{}/issues{tail}", self.api, self.repo)
    }

    fn request(&self, method: reqwest::Method, url: String) -> reqwest::RequestBuilder {
        self.client
            .request(method, url)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
    }

    async fn send(
        &self,
        what: &str,
        request: reqwest::RequestBuilder,
    ) -> Result<Option<Value>, Error> {
        let response = request.send().await.map_err(|e| surface_error(what, e))?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let text = response.text().await.map_err(|e| surface_error(what, e))?;
        if !status.is_success() {
            let excerpt: String = text.chars().take(300).collect();
            return Err(surface_error(what, format!("{status}: {excerpt}")));
        }
        if text.trim().is_empty() {
            return Ok(Some(Value::Null));
        }
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| surface_error(what, e))
    }

    fn state(p: &Projection) -> &'static str {
        if p.open {
            "open"
        } else {
            "closed"
        }
    }

    /// Add the projection's labels the issue lacks; never remove one.
    async fn ensure_labels(
        &self,
        number: &str,
        have: &[String],
        want: &[String],
    ) -> Result<(), Error> {
        let missing: Vec<&String> = want
            .iter()
            .filter(|l| !have.iter().any(|h| h.eq_ignore_ascii_case(l)))
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        let url = self.url(&format!("/{number}/labels"));
        self.send(
            "add labels",
            self.request(reqwest::Method::POST, url)
                .json(&json!({ "labels": missing })),
        )
        .await?;
        Ok(())
    }
}

#[async_trait]
impl ReviewSurface for GithubIssues {
    fn name(&self) -> &str {
        "github"
    }

    async fn issues(&self) -> Result<Vec<Issue>, Error> {
        let mut out: Vec<Issue> = Vec::new();
        for page in 1..=MAX_PAGES {
            let url = self.url(&format!(
                "?state=all&labels={LABEL_MANAGED}&per_page=100&page={page}"
            ));
            let Some(Value::Array(batch)) = self
                .send("list issues", self.request(reqwest::Method::GET, url))
                .await?
            else {
                break;
            };
            let before = out.len();
            for v in &batch {
                // The issues API lists pull requests too.
                if v.get("pull_request").is_some_and(|p| !p.is_null()) {
                    continue;
                }
                if let Some(issue) = issue_of(v) {
                    let managed = issue
                        .labels
                        .iter()
                        .any(|l| l.eq_ignore_ascii_case(LABEL_MANAGED));
                    if managed && !out.iter().any(|i| i.number == issue.number) {
                        out.push(issue);
                    }
                }
            }
            // A short page is the last; a page with nothing new means the
            // server ignored the page number.
            if batch.len() < 100 || out.len() == before {
                break;
            }
        }
        Ok(out)
    }

    async fn issue(&self, number: &str) -> Result<Option<Issue>, Error> {
        let url = self.url(&format!("/{number}"));
        Ok(self
            .send("read issue", self.request(reqwest::Method::GET, url))
            .await?
            .as_ref()
            .and_then(issue_of))
    }

    async fn create(&self, p: &Projection) -> Result<Issue, Error> {
        let created = self
            .send(
                "create issue",
                self.request(reqwest::Method::POST, self.url(""))
                    .json(&json!({ "title": p.title, "body": p.body, "labels": p.labels })),
            )
            .await?
            .as_ref()
            .and_then(issue_of)
            .ok_or_else(|| surface_error("create issue", "no issue in the reply"))?;
        if !p.open {
            return self.update(&created.number, p).await;
        }
        Ok(created)
    }

    async fn update(&self, number: &str, p: &Projection) -> Result<Issue, Error> {
        let url = self.url(&format!("/{number}"));
        let updated = self
            .send(
                "update issue",
                self.request(reqwest::Method::PATCH, url).json(&json!({
                    "title": p.title,
                    "body": p.body,
                    "state": Self::state(p),
                })),
            )
            .await?
            .as_ref()
            .and_then(issue_of)
            .ok_or_else(|| surface_error("update issue", format!("#{number} is gone")))?;
        self.ensure_labels(number, &updated.labels, &p.labels)
            .await?;
        Ok(updated)
    }

    async fn comments(&self, number: &str) -> Result<Vec<Comment>, Error> {
        let url = self.url(&format!("/{number}/comments?per_page=100"));
        let Some(Value::Array(all)) = self
            .send("list comments", self.request(reqwest::Method::GET, url))
            .await?
        else {
            return Ok(Vec::new());
        };
        Ok(all
            .iter()
            .filter_map(|c| {
                let id = match &c["id"] {
                    Value::Number(n) => n.to_string(),
                    Value::String(s) => s.clone(),
                    _ => return None,
                };
                let association = c["author_association"].as_str().unwrap_or_default();
                Some(Comment {
                    id,
                    author: c["user"]["login"].as_str().unwrap_or("unknown").to_string(),
                    trusted: matches!(association, "OWNER" | "MEMBER" | "COLLABORATOR"),
                    body: c["body"].as_str().unwrap_or_default().to_string(),
                })
            })
            .collect())
    }
}

/// The review surface this deployment is configured for, if any: GitHub
/// when `RUSTYKRAB_GITHUB_REPO` names a repository and a token resolves.
pub async fn review_surface(store: &Store) -> Option<Arc<dyn ReviewSurface>> {
    let repo = std::env::var("RUSTYKRAB_GITHUB_REPO")
        .ok()
        .map(|r| r.trim().to_string())
        .filter(|r| r.contains('/'))?;
    let spec = rustykrab_store::registry::REGISTRY
        .iter()
        .find(|s| s.store_name == "github_token")?;
    let Some(token) = rustykrab_store::registry::resolve(spec, &store.secrets()).await else {
        tracing::info!(
            repo = %repo,
            "review surface off: no github_token in the credential store"
        );
        return None;
    };
    let api = std::env::var("RUSTYKRAB_GITHUB_API_BASE")
        .ok()
        .filter(|a| !a.trim().is_empty())
        .unwrap_or_else(|| "https://api.github.com".to_string());
    match GithubIssues::new(&api, &repo, &token) {
        Ok(g) => {
            tracing::info!(repo = %repo, "review surface: GitHub issues");
            Some(Arc::new(g))
        }
        Err(e) => {
            tracing::warn!(error = %e, "review surface off");
            None
        }
    }
}

// ── the pass ───────────────────────────────────────────────────────────

/// One evaluation pass on demand or on the nightly timer.
pub struct Evaluator {
    store: Store,
    control: Arc<dyn ControlHandle>,
    evaluation: Evaluation,
    surface: Option<Arc<dyn ReviewSurface>>,
    running: tokio::sync::Mutex<()>,
}

impl Evaluator {
    /// Run one pass: decisions in, dreaming, projections out.
    pub async fn run(&self) -> Result<EvaluationReport, Error> {
        let _one_at_a_time = self.running.lock().await;
        let mut projection: Option<ProjectionReport> = None;
        let mut decisions = Vec::new();
        if let Some(surface) = &self.surface {
            match review::pull_decisions(&self.store, surface.as_ref(), self.control.as_ref()).await
            {
                Ok(pulled) => {
                    decisions = pulled.decisions;
                    projection = Some(pulled.report);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "review surface: decisions not read");
                    projection = Some(ProjectionReport {
                        errors: vec![e.to_string()],
                        ..ProjectionReport::default()
                    });
                }
            }
        }
        let mut report = self.evaluation.run(Utc::now()).await?;
        report.decisions = decisions;
        if let Some(surface) = &self.surface {
            let mut out = projection.unwrap_or_default();
            match review::push_projections(
                &self.store,
                surface.as_ref(),
                self.control.as_ref(),
                Utc::now(),
            )
            .await
            {
                Ok(pushed) => {
                    out.created = pushed.created;
                    out.updated = pushed.updated;
                    out.unchanged = pushed.unchanged;
                    out.errors.extend(pushed.errors);
                }
                Err(e) => out.errors.push(e.to_string()),
            }
            report.projection = Some(out);
        }
        Ok(report)
    }
}

impl EvaluationHandle for Evaluator {
    fn evaluate(&self) -> rustykrab_gateway::evaluate_routes::EvaluationFuture<'_> {
        Box::pin(self.run())
    }
}

/// Assemble the pass over this daemon's store and controller. The
/// questions and routing readers observe the durable question and worker tables.
pub async fn evaluator(
    store: &Store,
    control: Arc<dyn ControlHandle>,
    routing: Arc<dyn RoutingRecordReader>,
) -> Arc<Evaluator> {
    let config = EvaluationConfig::default();
    let outcomes = store.outcomes_reader().unwrap_or_else(|e| {
        tracing::warn!(error = %e, "evaluation reads outcomes on the shared connection");
        store.outcomes()
    });
    let evaluation = Evaluation {
        work: Arc::new(StoreWorkRecords::new(store.clone())),
        outcomes: Arc::new(StoreOutcomeSource::new(outcomes)),
        questions: Arc::new(StoreQuestions::new(store.clone())),
        routing,
        filer: Arc::new(ControlFiler {
            control: control.clone(),
            actor: config.actor.clone(),
        }),
        ledger: Arc::new(StoreLedger::new(store.clone())),
        config,
    };
    Arc::new(Evaluator {
        store: store.clone(),
        control,
        evaluation,
        surface: review_surface(store).await,
        running: tokio::sync::Mutex::new(()),
    })
}

/// The nightly timer: one pass every `RUSTYKRAB_EVALUATION_INTERVAL_SECS`
/// (a day by default; `0` turns the timer off, leaving the on-demand
/// route). The first pass waits one interval, so a restart does not
/// evaluate a daemon that has done nothing yet.
pub fn spawn_nightly(evaluator: Arc<Evaluator>) -> tokio::task::JoinHandle<()> {
    let secs: u64 = std::env::var("RUSTYKRAB_EVALUATION_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(24 * 3600);
    tokio::spawn(async move {
        if secs == 0 {
            tracing::info!("nightly evaluation off (RUSTYKRAB_EVALUATION_INTERVAL_SECS=0)");
            return;
        }
        let mut timer = tokio::time::interval(Duration::from_secs(secs));
        timer.tick().await;
        loop {
            timer.tick().await;
            match evaluator.run().await {
                Ok(report) => tracing::info!(
                    metrics = report.metrics.len(),
                    regressions = report.regressions.len(),
                    filed = report.filed.len(),
                    decisions = report.decisions.len(),
                    "evaluation pass"
                ),
                Err(e) => tracing::warn!(error = %e, "evaluation pass failed"),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(
        name: &str,
        kind: &str,
        tier: u32,
        live: bool,
        verified: u64,
        missed: u64,
    ) -> WorkerView {
        let mut record = rustykrab_store::RoutingRecord::new();
        record.insert(
            "code".to_string(),
            rustykrab_store::ClassRecord {
                verified_done: verified,
                claimed_not_verified: missed,
                recent_items: vec![format!("item-{name}")],
                cost: rustykrab_store::ClassCost {
                    runs: verified + missed,
                    wall_seconds: 0,
                    tokens: 1_000 * (verified + missed),
                },
                ..rustykrab_store::ClassRecord::default()
            },
        );
        WorkerView {
            name: name.into(),
            kind: kind.into(),
            live,
            healthy: live,
            health: "healthy".into(),
            last_seen: None,
            cost_tier: tier,
            concurrency: 1,
            capabilities: Default::default(),
            routing_record: record,
            spec: None,
            runtime: None,
            created_at: Utc::now(),
        }
    }

    /// Scenario 17's shape as dreaming reads it: the local worker holds
    /// code's default (tier 0), with its items and its cost; a gone worker
    /// never holds a default.
    #[test]
    fn routing_entries_name_the_default_holder_and_the_items_behind_a_record() {
        let workers = vec![
            view("gone", "local", 0, false, 0, 0),
            view("snapper", "local", 0, true, 1, 2),
            view("pinch", "claude_code", 3, true, 1, 0),
        ];
        let defaults = std::collections::BTreeMap::from([("code".to_string(), 0)]);
        let entries = routing_entries(&workers, &defaults);
        assert_eq!(entries.len(), 3);
        let snapper = entries.iter().find(|e| e.worker == "snapper").unwrap();
        assert!(snapper.default_for_class);
        assert_eq!(snapper.worker_kind, WorkerKind::Local);
        assert_eq!(
            (snapper.verified_done, snapper.claimed_not_verified),
            (1, 2)
        );
        assert_eq!(snapper.items, ["item-snapper"]);
        assert_eq!(snapper.cost, 1_000.0);
        assert_eq!(snapper.default_tier, Some(0));
        let pinch = entries.iter().find(|e| e.worker == "pinch").unwrap();
        assert!(!pinch.default_for_class);
        assert_eq!(pinch.cost_tier, 3);
        assert!(!entries
            .iter()
            .any(|e| e.worker == "gone" && e.default_for_class));

        // The default moved up: the coding agent holds it.
        let moved = std::collections::BTreeMap::from([("code".to_string(), 3)]);
        let entries = routing_entries(&workers, &moved);
        let holders: Vec<&str> = entries
            .iter()
            .filter(|e| e.default_for_class)
            .map(|e| e.worker.as_str())
            .collect();
        assert_eq!(holders, ["pinch"]);
        // A class with no default row has no holder.
        assert!(routing_entries(&workers, &Default::default())
            .iter()
            .all(|e| !e.default_for_class));
    }

    /// The GitHub stand-in's shapes and the real API's both parse.
    #[test]
    fn issues_and_labels_parse_from_either_shape() {
        let real = json!({
            "number": 7, "html_url": "https://github.com/o/r/issues/7",
            "title": "T", "body": null, "state": "closed",
            "labels": [ { "name": "rustykrab" }, { "name": "rustykrab-proposal" } ],
        });
        let issue = issue_of(&real).unwrap();
        assert_eq!(issue.number, "7");
        assert!(!issue.open);
        assert_eq!(issue.body, "");
        assert_eq!(issue.labels, vec!["rustykrab", "rustykrab-proposal"]);
        let stand_in =
            json!({ "number": 3, "title": "U", "labels": ["rustykrab"], "state": "open" });
        let issue = issue_of(&stand_in).unwrap();
        assert!(issue.open);
        assert_eq!(issue.labels, vec!["rustykrab"]);
        assert!(issue_of(&json!({ "title": "no number" })).is_none());
    }
}
