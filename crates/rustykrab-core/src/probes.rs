//! Concrete post-condition probes.
//!
//! Each one answers "did this effect appear?" by looking at the thing
//! itself, through a path that has nothing to do with what the agent
//! reported doing. That independence is the whole property — see
//! [`crate::post_condition`] for why a probe that consults the run's own
//! trace is not a probe.
//!
//! These two are deliberately the boring ones: a file on disk and a count
//! of stored memories. They are here to make the abstraction real and to
//! serve as the shape a calendar or mailbox probe should take, not because
//! they are the interesting effects. A probe for an external service
//! belongs with the crate that already owns its client.

use std::path::PathBuf;
use std::sync::Arc;

use crate::memory_backend::MemoryBackend;
use crate::post_condition::{Observation, PostCondition};
use crate::Result;

/// "A file exists at this path, and this run is what put it there."
///
/// The fingerprint is length and modification time, so rewriting a file
/// that already existed reads as the effect being produced again rather
/// than as nothing having happened.
pub struct FilePresence {
    name: String,
    path: PathBuf,
}

impl FilePresence {
    pub fn new(name: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            path: path.into(),
        }
    }
}

#[async_trait::async_trait]
impl PostCondition for FilePresence {
    fn name(&self) -> &str {
        &self.name
    }

    async fn observe(&self) -> Result<Observation> {
        let path = self.path.clone();
        let meta = tokio::task::spawn_blocking(move || std::fs::metadata(&path))
            .await
            .map_err(|e| crate::Error::Internal(format!("probe join failed: {e}")))?;

        Ok(match meta {
            // Absent is a legitimate observation, not an error: the effect
            // simply does not hold yet. Any *other* IO failure -- a
            // permission problem, a broken mount -- is a probe that could
            // not answer, and must not be reported as "the file is not
            // there".
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(crate::Error::Internal(format!(
                    "cannot observe {}: {e}",
                    self.path.display()
                )))
            }
            Ok(m) => {
                let mtime = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                Some(format!("{}:{}", m.len(), mtime))
            }
        })
    }
}

/// "The run committed something to memory."
///
/// Observes the store, not the `memory_save` tool call: a call that
/// returned Ok but wrote nothing looks identical from the trace and
/// different from here.
///
/// Counts by listing, which is fine at the scale a probe runs at — twice
/// per turn, and only for a skill that declared this check — but is the
/// obvious thing to replace with a `count()` on the backend if a skill
/// ever declares it against a large store.
pub struct MemoryWritten {
    name: String,
    backend: Arc<dyn MemoryBackend>,
}

impl MemoryWritten {
    pub fn new(name: impl Into<String>, backend: Arc<dyn MemoryBackend>) -> Self {
        Self {
            name: name.into(),
            backend,
        }
    }
}

#[async_trait::async_trait]
impl PostCondition for MemoryWritten {
    fn name(&self) -> &str {
        &self.name
    }

    async fn observe(&self) -> Result<Observation> {
        let listed = self.backend.list().await?;
        // The backend returns free-form JSON; the count is what matters,
        // and an array is what every implementation returns.
        let count = listed
            .as_array()
            .map(|a| a.len())
            .or_else(|| {
                listed
                    .get("memories")
                    .and_then(|m| m.as_array())
                    .map(|a| a.len())
            })
            .ok_or_else(|| {
                crate::Error::Internal("memory backend did not return a list".to_string())
            })?;

        // Zero memories is "the effect does not hold", not "no answer".
        Ok(if count == 0 {
            None
        } else {
            Some(count.to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::post_condition::{ProbeRegistry, ProbeWindow};

    async fn window(probes: &ProbeRegistry, check: &str, mutate: impl FnOnce()) -> ProbeWindow {
        let checks = vec![check.to_string()];
        let before = probes.sample(&checks).await;
        mutate();
        ProbeWindow {
            after: probes.sample(&checks).await,
            before,
        }
    }

    #[tokio::test]
    async fn a_file_that_appears_is_observed_as_produced() {
        let dir = std::env::temp_dir().join(format!("rk-probe-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("report.md");

        let probes =
            ProbeRegistry::new().with(Arc::new(FilePresence::new("report_written", &path)));
        let w = window(&probes, "report_written", || {
            std::fs::write(&path, "hello").unwrap()
        })
        .await;

        assert_eq!(w.produced("report_written"), Some(true));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_file_that_was_already_there_is_not_credited_to_the_run() {
        let dir = std::env::temp_dir().join(format!("rk-probe-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("report.md");
        std::fs::write(&path, "existing").unwrap();

        let probes =
            ProbeRegistry::new().with(Arc::new(FilePresence::new("report_written", &path)));
        let w = window(&probes, "report_written", || {}).await;

        assert_eq!(
            w.produced("report_written"),
            Some(false),
            "an effect that predates the run is not evidence about it"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_file_that_never_appears_is_observed_as_absent() {
        let path = std::env::temp_dir().join(format!("rk-probe-missing-{}", uuid::Uuid::new_v4()));
        let probes = ProbeRegistry::new().with(Arc::new(FilePresence::new("never", &path)));
        let w = window(&probes, "never", || {}).await;

        assert_eq!(
            w.produced("never"),
            Some(false),
            "absent is an answer, not a failure to answer"
        );
    }
}
