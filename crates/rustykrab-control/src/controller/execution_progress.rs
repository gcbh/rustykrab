//! Host observations of liveness and the concrete output of shared Git repairs.
//! Neither a heartbeat nor a changed file can close work; normal Git/result
//! verification still owns completion. No extra scheduler or persistence store.
use std::collections::{HashMap, HashSet};
use std::process::Stdio;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use rustykrab_core::work::{WorkItem, WorkItemId};
use rustykrab_core::Error;
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

use super::Controller;
use crate::errors::{FailureInput, ProviderProblem};
use crate::workspace::Workspace;

#[derive(Clone)]
pub(super) struct MergeProgress {
    workspace: Workspace,
    paths: Vec<String>,
    observed: Option<DateTime<Utc>>,
    changed: DateTime<Utc>,
    seen: HashSet<String>,
    unavailable: Option<String>,
}

impl Controller {
    pub(super) async fn merge_progress_for(
        &self,
        item: &WorkItem,
        workspace: Option<&Workspace>,
    ) -> Result<Option<MergeProgress>, Error> {
        let Some(workspace) = workspace else {
            return Ok(None);
        };
        Ok(self
            .merge_recovery(item)
            .await?
            .map(|recovery| MergeProgress {
                workspace: workspace.clone(),
                paths: recovery.conflict.paths,
                observed: None,
                changed: self.clock.now(),
                seen: HashSet::new(),
                unavailable: None,
            }))
    }

    pub(super) async fn observe_execution(
        &self,
    ) -> Result<HashMap<WorkItemId, FailureInput>, Error> {
        let now = self.clock.now();
        let ttl = i64::try_from(self.config.lease_ttl_seconds)
            .ok()
            .and_then(TimeDelta::try_seconds)
            .unwrap_or(TimeDelta::MAX);
        let cadence = TimeDelta::seconds((self.config.lease_ttl_seconds / 4).clamp(1, 60) as i64);
        let runs: Vec<_> = self
            .state()
            .runs
            .iter()
            .filter(|(_, run)| !run.reported && !run.handle.is_finished())
            .map(|(id, run)| {
                (
                    id.clone(),
                    run.run_id.clone(),
                    run.worker.clone(),
                    run.merge_progress.clone(),
                )
            })
            .collect();
        let mut stalled = HashMap::new();
        for (id, run_id, worker, goal) in runs {
            // Only actual adapter observations renew liveness. A model-written
            // transcript, a queued item, or a living process is insufficient.
            let activity = self.worker(&worker).and_then(|w| w.activity(&run_id));
            let renewed = {
                let mut state = self.state();
                state.runs.get_mut(&id).is_some_and(|run| {
                    if activity.is_some_and(|n| n > 0) && activity != run.activity {
                        run.activity = activity;
                        run.since = now;
                        true
                    } else {
                        false
                    }
                })
            };
            if renewed {
                self.store.work_lease_heartbeat(&id).await?;
            }
            let Some(mut goal) = goal else { continue };
            if goal.observed.is_none_or(|at| now - at >= cadence) {
                match merge_fingerprint(&goal.workspace, &goal.paths).await {
                    Ok(fingerprint) => {
                        // The first read establishes the baseline, not progress.
                        let first = goal.observed.is_none();
                        let fresh = goal.seen.insert(fingerprint);
                        if fresh && !first {
                            goal.changed = now;
                        }
                        goal.unavailable = None;
                    }
                    Err(why) => {
                        goal.unavailable = Some(why);
                    }
                }
                goal.observed = Some(now);
            }
            if now - goal.changed > ttl {
                let failure = match &goal.unavailable {
                    Some(why) => FailureInput::Raw { message: format!("cannot observe shared Git repair progress: {why}") },
                    None => FailureInput::Provider {
                        problem: ProviderProblem::Loop,
                        detail: format!("stalled shared Git repair: no new conflict-file content, index resolution or commit within {}s; inspection/output alone is not resolution progress. Partial workspace retained at {}", self.config.lease_ttl_seconds, goal.workspace.path.display()),
                    },
                };
                stalled.insert(id.clone(), failure);
            }
            if let Some(run) = self.state().runs.get_mut(&id) {
                run.merge_progress = Some(goal);
            }
        }
        Ok(stalled)
    }
}

/// Bounded read-only Git observations. No external diff, text conversion,
/// hooks, index refresh, or shell. Output is hashed in memory, never evidence
/// of correctness and never copied into a prompt. A failure is unknown progress.
async fn merge_fingerprint(ws: &Workspace, paths: &[String]) -> Result<String, String> {
    let mut values = Vec::new();
    values.push(git_read(ws, &["rev-parse", "--verify", "HEAD"], &[]).await?);
    values.push(git_read(ws, &["ls-files", "--unmerged", "-z", "--"], paths).await?);
    values.push(
        git_read(
            ws,
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--no-renames",
                "--ignore-submodules=all",
                "--binary",
                "HEAD",
                "--",
            ],
            paths,
        )
        .await?,
    );
    let mut hash = Sha256::new();
    for value in values {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value);
    }
    Ok(hex::encode(hash.finalize()))
}

async fn git_read(ws: &Workspace, args: &[&str], paths: &[String]) -> Result<Vec<u8>, String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C")
        .arg(&ws.path)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
        ])
        .args(args)
        .args(paths)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let operation = async {
        let mut child = cmd.spawn().map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        const MAX: u64 = 16 * 1024 * 1024;
        child
            .stdout
            .take()
            .ok_or("missing Git stdout")?
            .take(MAX + 1)
            .read_to_end(&mut out)
            .await
            .map_err(|e| e.to_string())?;
        if out.len() as u64 > MAX {
            return Err("Git progress output exceeds observation bound".into());
        }
        if !child.wait().await.map_err(|e| e.to_string())?.success() {
            return Err(format!("Git {} observation failed", args[0]));
        }
        Ok(out)
    };
    tokio::time::timeout(Duration::from_secs(5), operation)
        .await
        .map_err(|_| "Git progress observation timed out".to_string())?
}

#[cfg(test)]
#[path = "tests/execution_progress.rs"]
mod tests;
