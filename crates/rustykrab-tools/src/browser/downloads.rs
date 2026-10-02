//! Browser-session download observation.
//!
//! Chrome emits download lifecycle events at the browser connection rather
//! than the page session. One tracker is therefore attached to each managed
//! profile and survives tab changes. Page-controlled filenames are sanitized
//! at ingress, and file paths are exposed only after canonical containment in
//! the configured download directory has been verified.

use chromiumoxide::cdp::browser_protocol::browser::{
    DownloadProgressState, EventDownloadProgress, EventDownloadWillBegin,
    SetDownloadBehaviorBehavior, SetDownloadBehaviorParams,
};
use chromiumoxide::Browser;
use rustykrab_core::{Error, Result};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Notify};
use tokio_stream::StreamExt;

const MAX_DOWNLOAD_RECORDS: usize = 64;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadStatus {
    InProgress,
    Completed,
    Canceled,
}

#[derive(Debug, Clone, Serialize)]
pub struct DownloadRecord {
    pub sequence: u64,
    pub guid: String,
    pub filename: String,
    pub status: DownloadStatus,
    pub received_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub path_status: String,
}

#[derive(Default)]
struct DownloadState {
    sequence: u64,
    revision: u64,
    records: HashMap<String, DownloadRecord>,
    order: VecDeque<String>,
}

#[derive(Clone)]
pub struct DownloadTracker {
    root: PathBuf,
    state: Arc<Mutex<DownloadState>>,
    changed: Arc<Notify>,
}

impl DownloadTracker {
    /// Register listeners before enabling events so an immediate download
    /// cannot race tracker setup.
    pub async fn attach(
        browser: &Browser,
        root: PathBuf,
    ) -> Result<(Self, tokio::task::JoinHandle<()>)> {
        tokio::fs::create_dir_all(&root).await.map_err(|error| {
            Error::ToolExecution(
                format!(
                    "failed to create browser download directory '{}': {error}",
                    root.display()
                )
                .into(),
            )
        })?;
        let root = tokio::fs::canonicalize(&root).await.map_err(|error| {
            Error::ToolExecution(
                format!(
                    "failed to resolve browser download directory '{}': {error}",
                    root.display()
                )
                .into(),
            )
        })?;

        let begins = browser
            .event_listener::<EventDownloadWillBegin>()
            .await
            .map_err(|error| {
                Error::ToolExecution(
                    format!("failed to subscribe to download starts: {error}").into(),
                )
            })?;
        let progress = browser
            .event_listener::<EventDownloadProgress>()
            .await
            .map_err(|error| {
                Error::ToolExecution(
                    format!("failed to subscribe to download progress: {error}").into(),
                )
            })?;

        let params = SetDownloadBehaviorParams::builder()
            .behavior(SetDownloadBehaviorBehavior::Allow)
            .download_path(root.display().to_string())
            .events_enabled(true)
            .build()
            .map_err(|error| {
                Error::ToolExecution(format!("invalid Chrome download behavior: {error}").into())
            })?;
        browser.execute(params).await.map_err(|error| {
            Error::ToolExecution(format!("failed to enable Chrome downloads: {error}").into())
        })?;

        let tracker = Self {
            root,
            state: Arc::new(Mutex::new(DownloadState::default())),
            changed: Arc::new(Notify::new()),
        };
        let task_tracker = tracker.clone();
        let task = tokio::spawn(async move {
            let mut begins = begins;
            let mut progress = progress;
            loop {
                tokio::select! {
                    Some(event) = begins.next() => task_tracker.record_begin(&event).await,
                    Some(event) = progress.next() => task_tracker.record_progress(&event).await,
                    else => break,
                }
            }
        });
        Ok((tracker, task))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub async fn sequence(&self) -> u64 {
        self.state.lock().await.sequence
    }

    pub async fn list(&self) -> Vec<DownloadRecord> {
        let state = self.state.lock().await;
        state
            .order
            .iter()
            .filter_map(|guid| state.records.get(guid).cloned())
            .collect()
    }

    /// Observe downloads initiated after `since`. If a download starts, wait
    /// for all observed records to become terminal until the caller's bound.
    /// A timeout is an honest observation, never proof that the click failed.
    pub async fn observe_since(&self, since: u64, budget: Duration) -> serde_json::Value {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            let (records, observed_revision) = {
                let state = self.state.lock().await;
                (
                    state
                        .order
                        .iter()
                        .filter_map(|guid| state.records.get(guid))
                        .filter(|record| record.sequence > since)
                        .cloned()
                        .collect::<Vec<_>>(),
                    state.revision,
                )
            };
            if !records.is_empty()
                && records.iter().all(|record| {
                    matches!(
                        record.status,
                        DownloadStatus::Completed | DownloadStatus::Canceled
                    )
                })
            {
                let completed = records
                    .iter()
                    .filter(|record| matches!(record.status, DownloadStatus::Completed))
                    .count();
                return serde_json::json!({
                    "status": "terminal",
                    "completed": completed,
                    "downloads": records,
                });
            }

            let now = tokio::time::Instant::now();
            if now >= deadline {
                return serde_json::json!({
                    "status": if records.is_empty() { "not_observed" } else { "in_progress" },
                    "downloads": records,
                });
            }
            let notified = self.changed.notified();
            // Close the check/subscribe race: re-check the revision after
            // creating the notification future, then wait only if unchanged.
            if self.state.lock().await.revision != observed_revision {
                continue;
            }
            let _ = tokio::time::timeout_at(deadline, notified).await;
        }
    }

    async fn record_begin(&self, event: &EventDownloadWillBegin) {
        let mut state = self.state.lock().await;
        if state.records.contains_key(&event.guid) {
            return;
        }
        state.sequence = state.sequence.saturating_add(1);
        let sequence = state.sequence;
        state.order.push_back(event.guid.clone());
        state.records.insert(
            event.guid.clone(),
            DownloadRecord {
                sequence,
                guid: event.guid.clone(),
                filename: sanitize_filename(&event.suggested_filename),
                status: DownloadStatus::InProgress,
                received_bytes: 0,
                total_bytes: None,
                path: None,
                path_status: "pending".to_string(),
            },
        );
        state.revision = state.revision.saturating_add(1);
        trim_records(&mut state);
        drop(state);
        self.changed.notify_one();
    }

    async fn record_progress(&self, event: &EventDownloadProgress) {
        let mut state = self.state.lock().await;
        if !state.records.contains_key(&event.guid) {
            state.sequence = state.sequence.saturating_add(1);
            let sequence = state.sequence;
            state.order.push_back(event.guid.clone());
            state.records.insert(
                event.guid.clone(),
                DownloadRecord {
                    sequence,
                    guid: event.guid.clone(),
                    filename: "download".to_string(),
                    status: DownloadStatus::InProgress,
                    received_bytes: 0,
                    total_bytes: None,
                    path: None,
                    path_status: "event_started_without_metadata".to_string(),
                },
            );
        }

        let record = state.records.get_mut(&event.guid).expect("record inserted");
        record.received_bytes = nonnegative_bytes(event.received_bytes);
        let total = nonnegative_bytes(event.total_bytes);
        record.total_bytes = (total > 0).then_some(total);
        record.status = match event.state {
            DownloadProgressState::InProgress => DownloadStatus::InProgress,
            DownloadProgressState::Completed => DownloadStatus::Completed,
            DownloadProgressState::Canceled => DownloadStatus::Canceled,
        };

        if matches!(record.status, DownloadStatus::Completed) {
            let candidate = event
                .file_path
                .as_deref()
                .map(PathBuf::from)
                .unwrap_or_else(|| self.root.join(&record.filename));
            match validated_download_path(&self.root, &candidate) {
                Ok(path) => {
                    record.path = Some(path.display().to_string());
                    record.path_status = "validated".to_string();
                }
                Err(reason) => {
                    record.path = None;
                    record.path_status = reason;
                }
            }
        } else if matches!(record.status, DownloadStatus::Canceled) {
            record.path_status = "canceled".to_string();
        }
        state.revision = state.revision.saturating_add(1);
        trim_records(&mut state);
        drop(state);
        self.changed.notify_one();
    }
}

fn nonnegative_bytes(value: f64) -> u64 {
    if value.is_finite() && value > 0.0 {
        value.min(u64::MAX as f64) as u64
    } else {
        0
    }
}

fn trim_records(state: &mut DownloadState) {
    while state.order.len() > MAX_DOWNLOAD_RECORDS {
        if let Some(guid) = state.order.pop_front() {
            state.records.remove(&guid);
        }
    }
}

/// Treat the suggestion as untrusted display and filesystem input. Both slash
/// styles are normalized before taking the basename, control characters are
/// removed, and special/empty names receive a harmless fallback.
pub(crate) fn sanitize_filename(suggested: &str) -> String {
    let normalized = suggested.replace('\\', "/").replace('\0', "");
    let basename = normalized.rsplit('/').next().unwrap_or_default().trim();
    let mut safe: String = basename
        .chars()
        .filter(|ch| !ch.is_control() && !matches!(ch, '/' | '\\' | ':'))
        .take(180)
        .collect();
    while safe.ends_with('.') || safe.ends_with(' ') {
        safe.pop();
    }
    if safe.is_empty() || safe == "." || safe == ".." {
        "download".to_string()
    } else {
        safe
    }
}

fn validated_download_path(root: &Path, candidate: &Path) -> std::result::Result<PathBuf, String> {
    let canonical =
        std::fs::canonicalize(candidate).map_err(|_| "completed_path_unavailable".to_string())?;
    if !canonical.starts_with(root) {
        return Err("outside_download_directory".to_string());
    }
    if !canonical.is_file() {
        return Err("completed_path_not_a_file".to_string());
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_page_controlled_filenames() {
        for (input, expected) in [
            ("../../etc/passwd", "passwd"),
            (r"..\..\payload.exe", "payload.exe"),
            ("/absolute/report.pdf", "report.pdf"),
            ("report\0.pdf", "report.pdf"),
            (".", "download"),
            ("..", "download"),
            ("", "download"),
            ("invoice:2026.pdf", "invoice2026.pdf"),
        ] {
            assert_eq!(sanitize_filename(input), expected, "input={input:?}");
        }
    }

    #[test]
    fn validates_that_completed_paths_stay_inside_the_download_root() {
        let root = tempfile::tempdir().expect("download root");
        let inside = root.path().join("inside.txt");
        std::fs::write(&inside, b"ok").expect("inside file");
        let canonical_root = std::fs::canonicalize(root.path()).expect("canonical root");
        assert!(validated_download_path(&canonical_root, &inside).is_ok());

        let outside_dir = tempfile::tempdir().expect("outside root");
        let outside = outside_dir.path().join("outside.txt");
        std::fs::write(&outside, b"no").expect("outside file");
        assert_eq!(
            validated_download_path(&canonical_root, &outside).unwrap_err(),
            "outside_download_directory"
        );
    }
}
