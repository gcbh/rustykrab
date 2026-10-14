//! What is on record about a failure, for whoever investigates it.
//!
//! [`failure_history`] answers one question from the store: given a failure
//! fingerprint, or a work item, every occurrence. The items that hit it;
//! for each run of each item its worker, when it started and ended, how it
//! ended, the ladder rungs it climbed with their typed errors, the
//! evidence the controller recorded for it, the questions it surfaced with
//! their answers, the workspace it ran in and whether that worktree still
//! exists, and where its session log is. With them, every fix item that
//! was filed for the fingerprint: what it concluded, what it landed, and
//! each time the failure came back after it closed.
//!
//! One implementation serves three readers. An operator asks through
//! `rustykrab work investigate <fingerprint|item>` and
//! `GET /api/work/investigate/{target}`. The controller asks when it
//! leases a fix item and writes the answer into that run's investigation
//! bundle ([`bundle`]), because an external worker has no route to the
//! daemon's API. And the same fix records say what an earlier fix
//! concluded when the failure recurs.
//!
//! Nothing here calls a model. A run's records are attributed to it by the
//! run id where a record carries one (its `run` event, its `run_log`, its
//! `daemon_build`, its workspace's branch), and otherwise by time: a
//! record belongs to the run whose lease is the latest at or before it.
//! The controller stamps a lease when it starts a run and judges the
//! result on a later tick, so on the daemon's clock that attribution is
//! exact.
//!
//! [`redact`] takes secrets out of text copied for a worker, and
//! [`daemon_log`] cuts the daemon's own log to a run's window.

pub mod bundle;
pub mod daemon_log;
pub mod redact;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rustykrab_core::work::{
    ArtifactRef, EventKind, Evidence, Rung, RungEvent, Status, WorkError, WorkEvent, WorkItem,
    WorkItemId, WorkKind,
};
use rustykrab_core::Error;
use rustykrab_store::{QuestionFilter, Store, WorkFilter};
use serde::{Deserialize, Serialize};

use crate::errors::CLASSIFIER_RULE;
use crate::ladder;
use crate::worker::RUN_LOG;
use crate::workspace::{self, Workspace, WORKSPACE_EVIDENCE};

/// The artifact ref that marks an item as the fix item for a failure: its
/// value is the fingerprint.
pub const FAILURE_FINGERPRINT: &str = "failure_fingerprint";

/// The artifact ref a fix item carries when an earlier fix item for the
/// same fingerprint closed `done` before the failure came back: its value
/// is that item's id.
pub const PREVIOUS_FIX: &str = "previous_fix";

/// The evidence kind that records which build of the daemon ran a run:
/// `<version>+<commit>`, with the run id as its hash. Written at lease time.
pub const DAEMON_BUILD: &str = "daemon_build";

/// The evidence kind that records where a fix item's investigation bundle
/// was written, with the fingerprint as its hash.
pub const INVESTIGATION_BUNDLE: &str = "investigation_bundle";

/// What an `improve` rung's outcome starts with when the item was parked
/// behind a fix item; the fix item's id follows.
pub const RESOLUTION_WAIT: &str = "waiting on resolution item ";

/// The artifact ref a fix item carries when the failure it investigates is
/// in the daemon's own code: its value is the daemon's source repository,
/// which is also the item's `repo:` writable resource. Such an item runs in
/// a worktree of that repository at the running build's commit, and what it
/// commits stays on its own `rustykrab/work/<item>-<run>` branch there.
pub const DAEMON_SOURCE: &str = "daemon_source";

/// Occurrences returned unless the caller asks for another number.
pub const DEFAULT_LIMIT: usize = 50;

/// Characters kept of one evidence reference or event reason.
const TEXT_MAX: usize = 4_000;

const RUN: &str = "run";
const ERROR: &str = "error";
const SUMMARY: &str = "summary";

/// Evidence a run record shows through a field of its own, or that is a
/// frozen copy of the brief and too large to repeat.
const NOT_LISTED: [&str; 7] = [
    RUN,
    WORKSPACE_EVIDENCE,
    RUN_LOG,
    DAEMON_BUILD,
    INVESTIGATION_BUNDLE,
    "project_context",
    "work_discussion",
];

/// The fingerprint `item` is the fix item for, if the ladder filed it for
/// one: an `internal` item carrying a [`FAILURE_FINGERPRINT`] ref.
pub fn fix_fingerprint(kind: WorkKind, refs: &[ArtifactRef]) -> Option<&str> {
    if kind != WorkKind::Internal {
        return None;
    }
    refs.iter()
        .find(|r| r.kind == FAILURE_FINGERPRINT)
        .map(|r| r.value.as_str())
}

/// The daemon's own source checkout, when one of the repositories it was
/// configured with is the repository it was built from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceCheckout {
    pub repo: PathBuf,
    /// The commit the running daemon was built from, in full.
    pub commit: String,
    /// The running daemon's version.
    pub version: String,
    /// Whether it was built from a tree with uncommitted changes.
    pub dirty: bool,
}

/// The commit a daemon was built from, as its build script recorded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildId {
    /// The short or full commit hash.
    pub commit: String,
    pub dirty: bool,
}

impl BuildId {
    /// `<version>+<commit>[-dirty]`, the [`DAEMON_BUILD`] reference.
    pub fn label(build: Option<&BuildId>) -> String {
        match build {
            Some(b) if b.dirty => format!("{}+{}-dirty", rustykrab_core::VERSION, b.commit),
            Some(b) => format!("{}+{}", rustykrab_core::VERSION, b.commit),
            None => rustykrab_core::VERSION.to_string(),
        }
    }

    /// The commit a [`DAEMON_BUILD`] reference names, if it names one.
    pub fn commit_of(label: &str) -> Option<&str> {
        let commit = label.split_once('+')?.1;
        let commit = commit.strip_suffix("-dirty").unwrap_or(commit);
        (!commit.is_empty()).then_some(commit)
    }
}

/// A file a source tree has only if it is RustyKrab's.
const SOURCE_MARKER: &str = "crates/rustykrab-control/Cargo.toml";

/// The first of `candidates` that is the repository this daemon was built
/// from: it has the build's commit, and that commit has RustyKrab's own
/// control crate in it. Blocking (it runs git).
pub fn detect_source(candidates: &[PathBuf], build: &BuildId) -> Option<SourceCheckout> {
    candidates.iter().find_map(|repo| {
        let commit = workspace::resolve_commit(repo, &build.commit)
            .ok()
            .flatten()?;
        workspace::has_path(repo, &commit, SOURCE_MARKER)
            .ok()
            .filter(|has| *has)?;
        Some(SourceCheckout {
            repo: repo.clone(),
            commit,
            version: rustykrab_core::VERSION.to_string(),
            dirty: build.dirty,
        })
    })
}

/// What [`failure_history`] is told about the daemon it runs in.
#[derive(Debug, Clone, Default)]
pub struct HistoryContext {
    /// This daemon's [`DAEMON_BUILD`] label.
    pub build: Option<String>,
    /// The daemon's own source checkout, when known.
    pub source: Option<SourceCheckout>,
    /// Occurrences to return, the most recent first to be kept; zero means
    /// [`DEFAULT_LIMIT`].
    pub limit: usize,
}

/// Whether the query named a fingerprint or an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Matched {
    Fingerprint,
    Item,
}

/// Everything on record about a failure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FailureHistory {
    /// What was asked, as given.
    pub query: String,
    pub matched: Matched,
    /// The fingerprints the occurrences hit: the one asked for, or every
    /// one the item's runs climbed a rung for.
    pub fingerprints: Vec<FingerprintSummary>,
    /// One entry per item, oldest first.
    pub occurrences: Vec<Occurrence>,
    /// Items that hit the fingerprint, before [`HistoryContext::limit`].
    pub total_items: usize,
    /// The fix items filed for these fingerprints, oldest first.
    pub fixes: Vec<FixRecord>,
    pub generated_at: DateTime<Utc>,
    /// The build of the daemon that answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FingerprintSummary {
    pub fingerprint: String,
    pub class: String,
    pub subclass: String,
    /// The detail of its latest occurrence.
    pub detail: String,
    pub items: usize,
    /// Runs that hit it.
    pub runs: usize,
    pub first_seen: Option<DateTime<Utc>>,
    pub last_seen: Option<DateTime<Utc>>,
}

/// One item that hit the failure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Occurrence {
    pub item: ItemRecord,
    /// Its runs, oldest first.
    pub runs: Vec<RunRecord>,
    /// The questions it surfaced, with their answers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<QuestionRecord>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemRecord {
    pub id: WorkItemId,
    pub kind: WorkKind,
    pub title: String,
    pub status: Status,
    /// Aged out of the live table: only its archive line, events and
    /// evidence remain.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub archived: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub objective: Option<String>,
    /// The repository it writes, when it names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<DateTime<Utc>>,
}

/// One run of an item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRecord {
    /// The id the controller gave it. Empty for records older than run ids.
    pub run: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker: Option<String>,
    pub started_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<DateTime<Utc>>,
    /// What the controller recorded when the run ended: how, and what it
    /// spent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<String>,
    /// What the controller made of the run: the first status it gave the
    /// item once the run was judged (`done`, `blocked(verification_failed)`,
    /// `queued` for a retry). Absent while the run has not been judged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judged: Option<Status>,
    /// Whether this run hit the fingerprint asked for.
    pub hit: bool,
    /// The build of the daemon that ran it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    /// The ladder rungs climbed for it, each with its typed error.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rungs: Vec<RungEvent>,
    /// Errors, commands, checks, the summary, known limits and whatever
    /// else was recorded for it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<EvidenceRecord>,
    /// Every other event of the run, oldest first: the lease, the
    /// transitions, comments, questions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trail: Vec<TrailEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_log: Option<FileRecord>,
    /// The daemon's own log for the run's window. Only in a bundle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_log: Option<DaemonLogRecord>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceRecord {
    pub kind: String,
    pub reference: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_by: Option<String>,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrailEntry {
    pub at: DateTime<Utc>,
    pub kind: EventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<Status>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<Status>,
    pub actor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRecord {
    pub repository: PathBuf,
    pub branch: String,
    /// The commit the run started from.
    pub base: String,
    pub worktree: PathBuf,
    /// Whether the worktree directory is still there (a failed run's is
    /// kept until retention prunes it).
    pub worktree_exists: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRecord {
    pub path: PathBuf,
    pub exists: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonLogRecord {
    pub path: PathBuf,
    pub lines_in_window: usize,
    pub lines_kept: usize,
    pub redactions: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuestionRecord {
    pub id: String,
    pub kind: String,
    pub status: String,
    pub text: String,
    pub asked_by: String,
    pub asked_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered_at: Option<DateTime<Utc>>,
}

/// A fix item filed for the fingerprint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FixRecord {
    pub item: ItemRecord,
    /// What it landed that the controller checked: commits verified by
    /// git, classifier rules verified by replay. Empty: it landed nothing.
    pub landed: Vec<EvidenceRecord>,
    /// Its latest result summary: what it concluded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub known_limits: Vec<String>,
    /// Its own runs, with their session logs.
    pub runs: Vec<RunRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<QuestionRecord>,
    /// Runs that hit the fingerprint after this item closed `done`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recurrences: Vec<Recurrence>,
}

/// The failure coming back after a fix item closed `done`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recurrence {
    pub item: WorkItemId,
    pub run: String,
    pub at: DateTime<Utc>,
    /// The commit the recurring run started from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// The build of the daemon that ran it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    /// Whether what ran had the fix in it.
    pub includes_fix: Inclusion,
    /// What that verdict rests on, in words.
    pub basis: String,
}

/// Whether a recurring run's code had an earlier fix in it. A recurrence on
/// code without the fix says nothing about the fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Inclusion {
    Included,
    NotIncluded,
    Unknown,
}

fn clip(text: &str) -> String {
    match text.char_indices().nth(TEXT_MAX) {
        Some((cut, _)) => format!("{}... [cut]", &text[..cut]),
        None => text.to_string(),
    }
}

fn evidence_record(e: &Evidence) -> EvidenceRecord {
    EvidenceRecord {
        kind: e.kind.clone(),
        reference: clip(&e.reference),
        hash: e.hash.clone(),
        verified_by: e.verified_by.clone(),
        at: e.at,
    }
}

fn file_record(path: &Path) -> FileRecord {
    let meta = std::fs::metadata(path).ok().filter(|m| m.is_file());
    FileRecord {
        path: path.to_path_buf(),
        exists: meta.is_some(),
        bytes: meta.map(|m| m.len()),
    }
}

/// The runs of one item, read back from its events and evidence.
pub(crate) fn runs_of(
    events: &[WorkEvent],
    evidence: &[Evidence],
    fingerprint: Option<&str>,
) -> Vec<RunRecord> {
    let mut starts: Vec<(String, DateTime<Utc>)> = evidence
        .iter()
        .filter(|e| e.kind == RUN)
        .map(|e| (e.reference.clone(), e.at))
        .collect();
    starts.sort_by_key(|(_, at)| *at);
    if starts.is_empty() {
        // Records from before runs had ids: one unnamed run holds them all.
        let Some(first) = events
            .iter()
            .map(|e| e.at)
            .chain(evidence.iter().map(|e| e.at))
            .min()
        else {
            return Vec::new();
        };
        if !events.iter().any(|e| e.kind == EventKind::Lease) {
            return Vec::new();
        }
        starts.push((String::new(), first));
    }
    let mut runs = Vec::with_capacity(starts.len());
    for (i, (run, start)) in starts.iter().enumerate() {
        let end = starts.get(i + 1).map(|(_, at)| *at);
        let within = |at: DateTime<Utc>| at >= *start && end.is_none_or(|end| at < end);
        let mine: Vec<&WorkEvent> = events.iter().filter(|e| within(e.at)).collect();
        let ended = mine
            .iter()
            .find(|e| e.kind == EventKind::Run && e.evidence_ref.as_deref() == Some(run.as_str()))
            .or_else(|| mine.iter().find(|e| e.kind == EventKind::Run));
        let lease = mine.iter().find(|e| e.kind == EventKind::Lease);
        let worker = lease
            .map(|e| e.actor.as_str())
            .or(ended.map(|e| e.actor.as_str()))
            .map(|actor| actor.strip_prefix("worker:").unwrap_or(actor).to_string());
        let rungs: Vec<RungEvent> = mine.iter().filter_map(|e| ladder::rung_event(e)).collect();
        let trail: Vec<TrailEntry> = mine
            .iter()
            .filter(|e| e.kind != EventKind::Rung && e.kind != EventKind::Run)
            .map(|e| TrailEntry {
                at: e.at,
                kind: e.kind,
                from: e.from,
                to: e.to,
                actor: e.actor.clone(),
                reason: e.reason.as_deref().map(clip),
            })
            .collect();
        // What the controller made of the run: the first status it gave
        // the item after the run started, `verifying` aside. A failed run
        // that was then queued again for a repair reads as the failure.
        let after_start = mine
            .iter()
            .rposition(|e| e.to == Some(Status::Running))
            .map_or(0, |i| i + 1);
        let judged = mine[after_start..]
            .iter()
            .filter(|e| matches!(e.kind, EventKind::Transition | EventKind::Cascade))
            .filter_map(|e| e.to)
            .find(|status| *status != Status::Verifying);
        let own: Vec<&Evidence> = evidence.iter().filter(|e| within(e.at)).collect();
        let slug = Workspace::slug_of_run(run);
        let workspace = own
            .iter()
            .filter(|e| e.kind == WORKSPACE_EVIDENCE)
            .filter_map(|e| serde_json::from_str::<Workspace>(&e.reference).ok())
            .find(|ws| run.is_empty() || ws.branch.ends_with(&slug))
            .map(|ws| WorkspaceRecord {
                worktree_exists: ws.path.is_dir(),
                repository: ws.repo,
                branch: ws.branch,
                base: ws.base,
                worktree: ws.path,
            });
        let tagged = |kind: &str| {
            evidence
                .iter()
                .rev()
                .find(|e| e.kind == kind && e.hash.as_deref() == Some(run.as_str()))
        };
        let run_log = tagged(RUN_LOG).map(|e| file_record(Path::new(&e.reference)));
        let build = tagged(DAEMON_BUILD).map(|e| e.reference.clone());
        let hit = fingerprint.is_some_and(|fp| {
            rungs
                .iter()
                .any(|r| r.error.as_ref().is_some_and(|e| e.fingerprint == fp))
                || own
                    .iter()
                    .any(|e| e.kind == ERROR && e.hash.as_deref() == Some(fp))
        });
        runs.push(RunRecord {
            run: run.clone(),
            worker,
            started_at: lease.map_or(*start, |e| e.at.min(*start)),
            ended_at: ended.map(|e| e.at),
            ended: ended.and_then(|e| e.reason.clone()),
            judged,
            hit,
            build,
            rungs,
            evidence: own
                .iter()
                .filter(|e| !NOT_LISTED.contains(&e.kind.as_str()))
                .map(|e| evidence_record(e))
                .collect(),
            trail,
            workspace,
            run_log,
            daemon_log: None,
        });
    }
    runs
}

/// An item's record, from its live row or its archive line.
async fn item_record(
    store: &Store,
    id: &str,
) -> Result<Option<(ItemRecord, Vec<ArtifactRef>)>, Error> {
    if let Some(item) = store.work_get(id).await? {
        return Ok(Some((live_record(&item), item.artifact_refs)));
    }
    Ok(store.work_archive_get(id).await?.map(|a| {
        (
            ItemRecord {
                id: a.id,
                kind: a.kind,
                title: a.title,
                status: a.status,
                archived: true,
                objective: Some(a.summary),
                repository: None,
                created_at: None,
                closed_at: Some(a.closed_at),
            },
            Vec::new(),
        )
    }))
}

fn live_record(item: &WorkItem) -> ItemRecord {
    ItemRecord {
        id: item.id.clone(),
        kind: item.kind,
        title: item.title.clone(),
        status: item.status,
        archived: false,
        objective: Some(clip(&item.objective)),
        repository: Workspace::repo_of(&item.writable_resources),
        created_at: Some(item.created_at),
        closed_at: item.closed_at,
    }
}

async fn questions_of(store: &Store, item: &str) -> Result<Vec<QuestionRecord>, Error> {
    let mut rows = store
        .questions_list(&QuestionFilter {
            item: Some(item.to_string()),
            ..QuestionFilter::default()
        })
        .await?;
    rows.sort_by_key(|q| q.created_at);
    Ok(rows
        .into_iter()
        .map(|q| QuestionRecord {
            id: q.id,
            kind: q.kind.as_str().to_string(),
            status: q.status.as_str().to_string(),
            text: clip(&q.text),
            asked_by: q.asked_by,
            asked_at: q.created_at,
            answer: q.answer.as_deref().map(clip),
            answered_by: q.answered_by,
            answered_at: q.answered_at,
        })
        .collect())
}

struct Loaded {
    record: ItemRecord,
    refs: Vec<ArtifactRef>,
    events: Vec<WorkEvent>,
    evidence: Vec<Evidence>,
}

async fn load(store: &Store, id: &str) -> Result<Option<Loaded>, Error> {
    let events = store.work_events(id).await?;
    let evidence = store.work_evidence_list(id).await?;
    let (record, refs) = match item_record(store, id).await? {
        Some(found) => found,
        None if events.is_empty() && evidence.is_empty() => return Ok(None),
        // Events and evidence outlive both the live row and, should it be
        // pruned, the archive line.
        None => (
            ItemRecord {
                id: id.to_string(),
                kind: WorkKind::Task,
                title: "(no longer on record)".to_string(),
                status: Status::Failed,
                archived: true,
                objective: None,
                repository: None,
                created_at: None,
                closed_at: None,
            },
            Vec::new(),
        ),
    };
    Ok(Some(Loaded {
        record,
        refs,
        events,
        evidence,
    }))
}

/// The fingerprints an item's rungs and error evidence name, with the
/// latest error for each.
fn fingerprints_in(
    events: &[WorkEvent],
    evidence: &[Evidence],
) -> Vec<(String, Option<WorkError>)> {
    let mut seen: Vec<(String, Option<WorkError>)> = Vec::new();
    let mut note = |fp: &str, error: Option<WorkError>| {
        if fp.is_empty() {
            return;
        }
        match seen.iter_mut().find(|(f, _)| f == fp) {
            Some((_, slot)) => {
                if error.is_some() {
                    *slot = error;
                }
            }
            None => seen.push((fp.to_string(), error)),
        }
    };
    for e in evidence.iter().filter(|e| e.kind == ERROR) {
        if let Some(fp) = &e.hash {
            note(fp, None);
        }
    }
    for rung in ladder::ladder_of_events(events) {
        if let Some(error) = rung.error {
            let fp = error.fingerprint.clone();
            note(&fp, Some(error));
        }
    }
    seen
}

/// Whether a recurring run's code had the fix `commit` (made in `repo`) in
/// it, from what the run recorded.
fn inclusion(
    commit: &str,
    repo: &Path,
    run: &RunRecord,
    source: Option<&SourceCheckout>,
) -> (Inclusion, String) {
    let short = |sha: &str| sha.chars().take(12).collect::<String>();
    let mut facts: Vec<(bool, String)> = Vec::new();
    let mut unknown: Vec<String> = Vec::new();
    match &run.workspace {
        Some(ws) if same_repo(&ws.repository, repo) => {
            match workspace::is_ancestor(repo, commit, &ws.base) {
                Ok(true) => facts.push((
                    true,
                    format!(
                        "the run started from {}, which contains the fix commit {}",
                        short(&ws.base),
                        short(commit)
                    ),
                )),
                Ok(false) => facts.push((
                    false,
                    format!(
                        "the run started from {}, which does not contain the fix commit {}: the \
                         fix was not in the code that ran",
                        short(&ws.base),
                        short(commit)
                    ),
                )),
                Err(why) => unknown.push(format!("git could not compare the commits ({why})")),
            }
        }
        Some(ws) => unknown.push(format!(
            "the run worked in {}, not the fix's repository {}",
            ws.repository.display(),
            repo.display()
        )),
        None => unknown.push("the run had no workspace".to_string()),
    }
    if source.is_some_and(|s| same_repo(&s.repo, repo)) {
        match run.build.as_deref().and_then(BuildId::commit_of) {
            Some(built) => match workspace::is_ancestor(repo, commit, built) {
                Ok(true) => facts.push((
                    true,
                    format!(
                        "the daemon build that ran it ({}) contains the fix commit {}",
                        run.build.as_deref().unwrap_or_default(),
                        short(commit)
                    ),
                )),
                Ok(false) => facts.push((
                    false,
                    format!(
                        "the daemon build that ran it ({}) does not contain the fix commit {}: \
                         the fix was not deployed when this ran",
                        run.build.as_deref().unwrap_or_default(),
                        short(commit)
                    ),
                )),
                Err(why) => unknown.push(format!(
                    "git could not place the daemon build {built} against the fix ({why})"
                )),
            },
            None => unknown.push(
                "the fix is in the daemon's own source and the run recorded no daemon build"
                    .to_string(),
            ),
        }
    }
    let verdict = match (
        facts.iter().any(|(yes, _)| *yes),
        facts.iter().any(|(yes, _)| !*yes),
    ) {
        (true, false) => Inclusion::Included,
        (false, true) => Inclusion::NotIncluded,
        _ => Inclusion::Unknown,
    };
    let mut basis: Vec<String> = facts.into_iter().map(|(_, text)| text).collect();
    if verdict == Inclusion::Unknown {
        basis.extend(unknown);
    }
    (verdict, basis.join("; "))
}

pub(crate) fn same_repo(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    a == b || canon(a) == canon(b)
}

/// Everything on record about `query`: a failure fingerprint (or an
/// unambiguous prefix of one), or a work item's id. `None` when the store
/// knows neither.
pub async fn failure_history(
    store: &Store,
    query: &str,
    ctx: &HistoryContext,
) -> Result<Option<FailureHistory>, Error> {
    let wanted = query.trim().trim_start_matches('#');
    if wanted.is_empty() {
        return Ok(None);
    }
    let limit = if ctx.limit == 0 {
        DEFAULT_LIMIT
    } else {
        ctx.limit
    };
    let errors = store.work_evidence_of_kind(ERROR).await?;

    // An item id wins over a fingerprint: ids are UUIDs, fingerprints hex.
    let (matched, item_ids, wanted_fp): (Matched, Vec<WorkItemId>, Option<String>) =
        if load(store, wanted).await?.is_some() {
            (Matched::Item, vec![wanted.to_string()], None)
        } else {
            let mut known: Vec<&str> = errors
                .iter()
                .filter_map(|e| e.hash.as_deref())
                .filter(|fp| fp.starts_with(wanted))
                .collect();
            known.sort_unstable();
            known.dedup();
            let fp = match known.as_slice() {
                [] => return Ok(None),
                [one] => one.to_string(),
                many if many.contains(&wanted) => wanted.to_string(),
                many => {
                    return Err(Error::NotFound(format!(
                        "`{wanted}` is the start of {} fingerprints: {}",
                        many.len(),
                        many.join(", ")
                    )))
                }
            };
            let mut ids: Vec<WorkItemId> = Vec::new();
            for e in errors.iter().filter(|e| e.hash.as_deref() == Some(&fp)) {
                if !ids.contains(&e.item) {
                    ids.push(e.item.clone());
                }
            }
            (Matched::Fingerprint, ids, Some(fp))
        };
    let total_items = item_ids.len();
    let kept: Vec<WorkItemId> = item_ids
        .iter()
        .skip(total_items.saturating_sub(limit))
        .cloned()
        .collect();

    let mut occurrences = Vec::with_capacity(kept.len());
    let mut summaries: Vec<FingerprintSummary> = Vec::new();
    let mut fix_ids: Vec<WorkItemId> = Vec::new();
    for id in &kept {
        let Some(loaded) = load(store, id).await? else {
            continue;
        };
        let named = fingerprints_in(&loaded.events, &loaded.evidence);
        let runs = runs_of(&loaded.events, &loaded.evidence, wanted_fp.as_deref());
        for (fp, error) in &named {
            if wanted_fp.as_deref().is_some_and(|w| w != fp) {
                continue;
            }
            let (class, subclass, detail) = match error {
                Some(e) => (
                    e.class.as_str().to_string(),
                    e.subclass.as_str().to_string(),
                    clip(&e.detail),
                ),
                None => described(&loaded.evidence, fp),
            };
            let stamps: Vec<DateTime<Utc>> = loaded
                .evidence
                .iter()
                .filter(|e| e.kind == ERROR && e.hash.as_deref() == Some(fp.as_str()))
                .map(|e| e.at)
                .collect();
            let hit_runs = match &wanted_fp {
                Some(_) => runs.iter().filter(|r| r.hit).count(),
                None => runs_of(&loaded.events, &loaded.evidence, Some(fp))
                    .iter()
                    .filter(|r| r.hit)
                    .count(),
            };
            match summaries.iter_mut().find(|s| &s.fingerprint == fp) {
                Some(s) => {
                    s.items += 1;
                    s.runs += hit_runs;
                    s.first_seen = s.first_seen.into_iter().chain(stamps.iter().copied()).min();
                    let latest = stamps.iter().copied().max();
                    if latest > s.last_seen {
                        s.last_seen = latest;
                        s.detail = detail;
                    }
                }
                None => summaries.push(FingerprintSummary {
                    fingerprint: fp.clone(),
                    class,
                    subclass,
                    detail,
                    items: 1,
                    runs: hit_runs,
                    first_seen: stamps.iter().copied().min(),
                    last_seen: stamps.iter().copied().max(),
                }),
            }
        }
        // The fix items this item was parked behind.
        for rung in ladder::ladder_of_events(&loaded.events) {
            if rung.rung != Rung::Improve {
                continue;
            }
            let about = rung.error.as_ref().map(|e| e.fingerprint.as_str());
            if wanted_fp.as_deref().is_some_and(|w| Some(w) != about) {
                continue;
            }
            if let Some(fix) = rung.outcome.strip_prefix(RESOLUTION_WAIT) {
                let fix = fix.trim().to_string();
                if !fix.is_empty() && !fix_ids.contains(&fix) {
                    fix_ids.push(fix);
                }
            }
        }
        // In item mode, a fix item's own fingerprint is one of interest.
        if matched == Matched::Item {
            if let Some(fp) = fix_fingerprint(loaded.record.kind, &loaded.refs) {
                if !summaries.iter().any(|s| s.fingerprint == fp) {
                    let (class, subclass, detail) = (String::new(), String::new(), String::new());
                    summaries.push(FingerprintSummary {
                        fingerprint: fp.to_string(),
                        class,
                        subclass,
                        detail,
                        items: 0,
                        runs: 0,
                        first_seen: None,
                        last_seen: None,
                    });
                }
            }
        }
        occurrences.push(Occurrence {
            questions: questions_of(store, id).await?,
            item: loaded.record,
            runs,
        });
    }

    // Fix items still on the live table name their fingerprint themselves.
    let wanted_fps: Vec<String> = summaries.iter().map(|s| s.fingerprint.clone()).collect();
    let live = store
        .work_list(&WorkFilter {
            kind: Some(WorkKind::Internal),
            include_closed: true,
            ..WorkFilter::default()
        })
        .await?;
    for item in &live {
        let Some(fp) = fix_fingerprint(item.kind, &item.artifact_refs) else {
            continue;
        };
        if wanted_fps.iter().any(|w| w == fp) && !fix_ids.contains(&item.id) {
            fix_ids.push(item.id.clone());
        }
    }

    let mut fixes = Vec::new();
    for id in &fix_ids {
        let Some(loaded) = load(store, id).await? else {
            continue;
        };
        let fp = fix_fingerprint(loaded.record.kind, &loaded.refs)
            .map(str::to_string)
            .or_else(|| wanted_fp.clone());
        let runs = runs_of(&loaded.events, &loaded.evidence, None);
        let landed: Vec<EvidenceRecord> = loaded
            .evidence
            .iter()
            .filter(|e| {
                (e.kind == "commit" || e.kind == CLASSIFIER_RULE) && e.verified_by.is_some()
            })
            .map(evidence_record)
            .collect();
        let conclusion = loaded
            .evidence
            .iter()
            .rev()
            .find(|e| e.kind == SUMMARY)
            .map(|e| clip(&e.reference));
        let known_limits = loaded
            .evidence
            .iter()
            .filter(|e| e.kind == "known_limit")
            .map(|e| clip(&e.reference))
            .collect();
        let fix_repo = runs
            .iter()
            .rev()
            .find_map(|r| r.workspace.as_ref().map(|ws| ws.repository.clone()));
        let closed_done = (loaded.record.status == Status::Done)
            .then_some(loaded.record.closed_at)
            .flatten();
        let mut recurrences = Vec::new();
        if let (Some(closed), Some(fp)) = (closed_done, fp.as_deref()) {
            let commit = landed.iter().rev().find(|e| e.kind == "commit");
            for occurrence in &occurrences {
                // A run counts when the queried fingerprint hit it; in item
                // mode `hit` was not computed for this fingerprint.
                let hits = occurrence.runs.iter().filter(|r| {
                    r.started_at > closed
                        && (r.hit
                            || r.rungs
                                .iter()
                                .any(|g| g.error.as_ref().is_some_and(|e| e.fingerprint == fp)))
                });
                for run in hits {
                    let (includes_fix, basis) = match (commit, &fix_repo) {
                        (Some(commit), Some(repo)) => {
                            let (commit, repo, run, source) = (
                                commit.reference.clone(),
                                repo.clone(),
                                run.clone(),
                                ctx.source.clone(),
                            );
                            tokio::task::spawn_blocking(move || {
                                inclusion(&commit, &repo, &run, source.as_ref())
                            })
                            .await
                            .map_err(|e| Error::Internal(e.to_string()))?
                        }
                        _ if landed.iter().any(|e| e.kind == CLASSIFIER_RULE) => (
                            Inclusion::Included,
                            "the fix was a classifier rule, which is in force from the moment \
                             the controller accepts it"
                                .to_string(),
                        ),
                        _ => (
                            Inclusion::Unknown,
                            "the earlier fix item recorded no verified commit to compare against"
                                .to_string(),
                        ),
                    };
                    recurrences.push(Recurrence {
                        item: occurrence.item.id.clone(),
                        run: run.run.clone(),
                        at: run.started_at,
                        base: run.workspace.as_ref().map(|ws| ws.base.clone()),
                        build: run.build.clone(),
                        includes_fix,
                        basis,
                    });
                }
            }
        }
        fixes.push(FixRecord {
            questions: questions_of(store, id).await?,
            item: loaded.record,
            landed,
            conclusion,
            known_limits,
            runs,
            recurrences,
        });
    }
    fixes.sort_by_key(|f| f.item.created_at);

    Ok(Some(FailureHistory {
        query: query.trim().to_string(),
        matched,
        fingerprints: summaries,
        occurrences,
        total_items,
        fixes,
        generated_at: Utc::now(),
        build: ctx.build.clone(),
    }))
}

/// Class, subclass and detail read back from an `error` evidence row's
/// reference, `<class>/<subclass>: <detail>`.
fn described(evidence: &[Evidence], fingerprint: &str) -> (String, String, String) {
    let text = evidence
        .iter()
        .rev()
        .find(|e| e.kind == ERROR && e.hash.as_deref() == Some(fingerprint))
        .map(|e| e.reference.as_str())
        .unwrap_or_default();
    let (head, detail) = text.split_once(": ").unwrap_or((text, ""));
    let (class, subclass) = head.split_once('/').unwrap_or((head, ""));
    (class.to_string(), subclass.to_string(), clip(detail))
}

/// A history as an operator reads it in a terminal: the fingerprint, each
/// occurrence with its runs, then the fix items.
pub fn render(history: &FailureHistory) -> String {
    use std::fmt::Write as _;
    let short = |id: &str| id.chars().take(8).collect::<String>();
    let when = |at: DateTime<Utc>| at.format("%Y-%m-%d %H:%M:%SZ").to_string();
    let mut out = String::new();
    for f in &history.fingerprints {
        let _ = writeln!(
            out,
            "failure {}  {}/{}  {} item(s), {} run(s)",
            f.fingerprint, f.class, f.subclass, f.items, f.runs
        );
        if let (Some(first), Some(last)) = (f.first_seen, f.last_seen) {
            let _ = writeln!(
                out,
                "  first seen {}, last seen {}",
                when(first),
                when(last)
            );
        }
        if !f.detail.is_empty() {
            let _ = writeln!(out, "  {}", f.detail);
        }
    }
    if history.fingerprints.is_empty() {
        let _ = writeln!(out, "no failure is recorded for {}", history.query);
    }
    if history.total_items > history.occurrences.len() {
        let _ = writeln!(
            out,
            "showing the latest {} of {} items",
            history.occurrences.len(),
            history.total_items
        );
    }
    let run_lines = |out: &mut String, runs: &[RunRecord]| {
        for run in runs {
            let _ = writeln!(
                out,
                "  run {}  {}  {} -> {}{}",
                if run.run.is_empty() {
                    "(unnamed)"
                } else {
                    &run.run
                },
                run.worker.as_deref().unwrap_or("?"),
                when(run.started_at),
                run.ended_at.map_or("still running".to_string(), when),
                if run.hit { "  HIT" } else { "" },
            );
            if let Some(status) = run.judged {
                let _ = writeln!(out, "    judged {status}");
            }
            if let Some(ended) = &run.ended {
                let _ = writeln!(out, "    {ended}");
            }
            if let Some(build) = &run.build {
                let _ = writeln!(out, "    daemon build {build}");
            }
            for rung in &run.rungs {
                let error = rung.error.as_ref().map_or(String::new(), |e| {
                    format!(
                        " {}/{} [{}]",
                        e.class.as_str(),
                        e.subclass.as_str(),
                        e.fingerprint
                    )
                });
                let _ = writeln!(
                    out,
                    "    rung {}{error}: {}",
                    rung.rung.as_str(),
                    first_line(&rung.outcome)
                );
            }
            for e in &run.evidence {
                let _ = writeln!(out, "    {}: {}", e.kind, first_line(&e.reference));
            }
            if let Some(ws) = &run.workspace {
                let _ = writeln!(
                    out,
                    "    workspace {} branch {} base {}",
                    ws.repository.display(),
                    ws.branch,
                    ws.base
                );
                let _ = writeln!(
                    out,
                    "    worktree {} ({})",
                    ws.worktree.display(),
                    if ws.worktree_exists {
                        "kept"
                    } else {
                        "removed"
                    }
                );
            }
            if let Some(log) = &run.run_log {
                let _ = writeln!(
                    out,
                    "    session log {}{}",
                    log.path.display(),
                    if log.exists { "" } else { " (no longer there)" }
                );
            }
        }
    };
    for occurrence in &history.occurrences {
        let item = &occurrence.item;
        let _ = writeln!(
            out,
            "\nitem #{}  {}  {}  \"{}\"{}",
            short(&item.id),
            item.kind.as_str(),
            item.status,
            item.title,
            if item.archived { "  (archived)" } else { "" }
        );
        let _ = writeln!(out, "  id {}", item.id);
        run_lines(&mut out, &occurrence.runs);
        for q in &occurrence.questions {
            let _ = writeln!(out, "  question ({}): {}", q.status, first_line(&q.text));
            if let Some(answer) = &q.answer {
                let _ = writeln!(
                    out,
                    "    answered by {}: {}",
                    q.answered_by.as_deref().unwrap_or("?"),
                    first_line(answer)
                );
            }
        }
    }
    for fix in &history.fixes {
        let _ = writeln!(
            out,
            "\nfix item #{}  {}  \"{}\"",
            short(&fix.item.id),
            fix.item.status,
            fix.item.title
        );
        let _ = writeln!(out, "  id {}", fix.item.id);
        match fix.landed.as_slice() {
            [] => {
                let _ = writeln!(out, "  landed nothing the controller checked");
            }
            landed => {
                for e in landed {
                    let _ = writeln!(
                        out,
                        "  landed {} {} (verified by {})",
                        e.kind,
                        e.reference,
                        e.verified_by.as_deref().unwrap_or("?")
                    );
                }
            }
        }
        if let Some(conclusion) = &fix.conclusion {
            let _ = writeln!(out, "  concluded: {}", first_line(conclusion));
        }
        for r in &fix.recurrences {
            let _ = writeln!(
                out,
                "  recurred on run {} of item #{} at {}: fix {} ({})",
                r.run,
                short(&r.item),
                when(r.at),
                match r.includes_fix {
                    Inclusion::Included => "was in what ran",
                    Inclusion::NotIncluded => "was not in what ran",
                    Inclusion::Unknown => "may or may not have been in what ran",
                },
                r.basis
            );
        }
        run_lines(&mut out, &fix.runs);
    }
    out
}

fn first_line(text: &str) -> String {
    let line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    match line.char_indices().nth(240) {
        Some((cut, _)) => format!("{}...", &line[..cut]),
        None => line.to_string(),
    }
}

/// The session log directories a history's runs name, for a worker's read
/// access.
pub(crate) fn log_dirs(history: &FailureHistory) -> Vec<PathBuf> {
    let mut dirs: BTreeMap<PathBuf, ()> = BTreeMap::new();
    let runs = history
        .occurrences
        .iter()
        .flat_map(|o| &o.runs)
        .chain(history.fixes.iter().flat_map(|f| &f.runs));
    for run in runs {
        if let Some(dir) = run
            .run_log
            .as_ref()
            .filter(|log| log.exists)
            .and_then(|log| log.path.parent())
        {
            dirs.insert(dir.to_path_buf(), ());
        }
    }
    dirs.into_keys().collect()
}
