//! Scripted stand-in tools for the evaluation harness.
//!
//! `ScriptedProvider` scripts the *model* so the plumbing can be tested
//! without one. This is the mirror image: it scripts the *tools* so a real
//! model can be tested against situations that a live tool would only
//! reach by luck — an upstream that times out once and then works, one
//! that never works, a search that legitimately returns nothing, a result
//! far larger than the context window.
//!
//! Enabled at daemon startup with `RUSTYKRAB_TOOL_STUBS=<file.json>`:
//!
//! ```json
//! {
//!   "mode": "replace",
//!   "keep": ["memory_search", "memory_save"],
//!   "tools": [
//!     {
//!       "name": "weather_lookup",
//!       "description": "Get the current weather for a city.",
//!       "parameters": { "type": "object",
//!                       "properties": { "city": { "type": "string" } },
//!                       "required": ["city"] },
//!       "script": {
//!         "responses": [
//!           { "type": "err", "message": "upstream timed out", "kind": "timeout" },
//!           { "type": "ok", "value": { "temperature_c": 17 } }
//!         ]
//!       }
//!     }
//!   ]
//! }
//! ```
//!
//! `mode: "replace"` leaves the model with only the stubs plus anything
//! named in `keep`. That is usually what an eval wants: thirty real tools
//! give a small model thirty ways to wander off, and the case stops
//! measuring what it meant to.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use rustykrab_core::error::{Result, ToolError, ToolErrorKind};
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Error, Tool};

/// What a stubbed tool hands back for one call.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StubResponse {
    /// Succeed with this JSON payload.
    Ok { value: Value },
    /// Fail. The runner surfaces this to the model as an `is_error` tool
    /// result, which is what drives the retry and reflection paths.
    Err {
        message: String,
        #[serde(default)]
        kind: StubErrorKind,
    },
    /// Succeed with a deliberately oversized payload: `header` followed by
    /// `line` repeated `lines` times. Pushes a conversation past the
    /// compaction trigger without waiting for a real 100k-token result.
    Filler {
        #[serde(default)]
        header: String,
        line: String,
        lines: usize,
    },
    /// Sleep, then deliver the inner response.
    Delay { ms: u64, then: Box<StubResponse> },
}

impl StubResponse {
    fn label(&self) -> &'static str {
        match self {
            StubResponse::Ok { .. } => "ok",
            StubResponse::Err { .. } => "error",
            StubResponse::Filler { .. } => "ok:filler",
            StubResponse::Delay { .. } => "ok:delayed",
        }
    }
}

/// Error classes a stub can raise, mirroring [`ToolErrorKind`]. The agent
/// loop treats these differently, so the kind is not cosmetic.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StubErrorKind {
    InvalidInput,
    NotFound,
    PermissionDenied,
    Timeout,
    RateLimited,
    Transient,
    #[default]
    Internal,
}

impl From<StubErrorKind> for ToolErrorKind {
    fn from(k: StubErrorKind) -> Self {
        match k {
            StubErrorKind::InvalidInput => ToolErrorKind::InvalidInput,
            StubErrorKind::NotFound => ToolErrorKind::NotFound,
            StubErrorKind::PermissionDenied => ToolErrorKind::PermissionDenied,
            StubErrorKind::Timeout => ToolErrorKind::Timeout,
            StubErrorKind::RateLimited => ToolErrorKind::RateLimited,
            StubErrorKind::Transient => ToolErrorKind::Transient,
            StubErrorKind::Internal => ToolErrorKind::Internal,
        }
    }
}

/// The ordered responses a stubbed tool gives across a run.
#[derive(Debug, Clone, Deserialize)]
pub struct StubScript {
    /// Response for call 1, call 2, and so on.
    pub responses: Vec<StubResponse>,
    /// When the model calls more times than the script has entries, repeat
    /// the last response rather than failing — most stubs are steady-state
    /// after their scripted opening. Set false to make further calls fail,
    /// which is how a case checks that an agent stops retrying.
    #[serde(default = "default_true")]
    pub repeat_last: bool,
}

fn default_true() -> bool {
    true
}

/// Declarative definition of one stubbed tool.
#[derive(Debug, Clone, Deserialize)]
pub struct StubSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the arguments, exactly as a real tool would declare
    /// it — whether the model can fill this in correctly is half of what a
    /// tool scenario measures.
    pub parameters: Value,
    pub script: StubScript,
}

/// How the stub file interacts with the daemon's real tool registry.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StubMode {
    /// Expose only the stubs plus anything in `keep`.
    #[default]
    Replace,
    /// Keep every real tool and add the stubs alongside. A stub whose name
    /// matches a real tool shadows it.
    Augment,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StubFile {
    #[serde(default)]
    pub mode: StubMode,
    /// Real tools to keep when `mode` is `replace`.
    #[serde(default)]
    pub keep: Vec<String>,
    pub tools: Vec<StubSpec>,
}

impl StubFile {
    pub fn from_path(path: &std::path::Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("reading tool stubs {}: {e}", path.display())))?;
        let parsed: Self = serde_json::from_str(&raw)
            .map_err(|e| Error::Config(format!("parsing tool stubs {}: {e}", path.display())))?;
        if parsed.tools.is_empty() && parsed.mode == StubMode::Replace {
            return Err(Error::Config(format!(
                "{} replaces the tool registry with nothing — the agent would have no tools",
                path.display()
            )));
        }
        Ok(parsed)
    }

    /// Apply this file to the daemon's tool list.
    pub fn apply(&self, real: Vec<Arc<dyn Tool>>) -> Vec<Arc<dyn Tool>> {
        let stub_names: Vec<&str> = self.tools.iter().map(|s| s.name.as_str()).collect();
        let mut tools: Vec<Arc<dyn Tool>> = match self.mode {
            StubMode::Replace => real
                .into_iter()
                .filter(|t| self.keep.iter().any(|k| k == t.name()))
                .collect(),
            // A stub shadows a real tool of the same name rather than
            // sitting beside it — two tools with one name is a schema the
            // model cannot choose between.
            StubMode::Augment => real
                .into_iter()
                .filter(|t| !stub_names.contains(&t.name()))
                .collect(),
        };
        tools.extend(
            self.tools
                .iter()
                .map(|spec| Arc::new(StubTool::new(spec)) as Arc<dyn Tool>),
        );
        tools
    }
}

/// A tool whose answers the eval author writes, rather than the world.
pub struct StubTool {
    name: String,
    description: String,
    parameters: Value,
    script: StubScript,
    calls: AtomicUsize,
}

impl StubTool {
    pub fn new(spec: &StubSpec) -> Self {
        Self {
            name: spec.name.clone(),
            description: spec.description.clone(),
            parameters: spec.parameters.clone(),
            script: spec.script.clone(),
            calls: AtomicUsize::new(0),
        }
    }

    fn response_for(&self, n: usize) -> Option<&StubResponse> {
        match self.script.responses.get(n) {
            Some(r) => Some(r),
            None if self.script.repeat_last => self.script.responses.last(),
            None => None,
        }
    }
}

#[async_trait]
impl Tool for StubTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let Some(response) = self.response_for(n).cloned() else {
            return Err(Error::ToolExecution(ToolError {
                kind: ToolErrorKind::Internal,
                message: format!(
                    "{}: stub script exhausted after {} call(s)",
                    self.name,
                    self.script.responses.len()
                ),
            }));
        };
        tracing::info!(
            tool = %self.name,
            call = n + 1,
            outcome = response.label(),
            args = %args,
            "stub tool call"
        );
        deliver(response).await
    }
}

/// Resolve one scripted response into a tool result.
async fn deliver(response: StubResponse) -> Result<Value> {
    match response {
        StubResponse::Ok { value } => Ok(value),
        StubResponse::Err { message, kind } => Err(Error::ToolExecution(ToolError {
            kind: kind.into(),
            message,
        })),
        StubResponse::Filler {
            header,
            line,
            lines,
        } => {
            let mut body = String::with_capacity(header.len() + (line.len() + 8) * lines);
            if !header.is_empty() {
                body.push_str(&header);
                body.push('\n');
            }
            for i in 0..lines {
                body.push_str(&format!("{i:04}  {line}\n"));
            }
            Ok(json!({ "content": body, "lines": lines }))
        }
        StubResponse::Delay { ms, then } => {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            Box::pin(deliver(*then)).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(responses: &str) -> StubSpec {
        serde_json::from_str(&format!(
            r#"{{"name":"probe","description":"a probe",
                 "parameters":{{"type":"object"}},
                 "script":{{"responses":{responses}}}}}"#
        ))
        .unwrap()
    }

    #[tokio::test]
    async fn walks_the_script_then_repeats_the_last_response() {
        let tool = StubTool::new(&spec(
            r#"[{"type":"err","message":"boom","kind":"transient"},
                {"type":"ok","value":{"value":7}}]"#,
        ));
        assert!(tool.execute(json!({})).await.is_err());
        assert_eq!(tool.execute(json!({})).await.unwrap()["value"], 7);
        assert_eq!(tool.execute(json!({})).await.unwrap()["value"], 7);
    }

    #[tokio::test]
    async fn a_non_repeating_script_errors_when_it_runs_out() {
        let mut s = spec(r#"[{"type":"ok","value":{}}]"#);
        s.script.repeat_last = false;
        let tool = StubTool::new(&s);
        assert!(tool.execute(json!({})).await.is_ok());
        let err = tool.execute(json!({})).await.unwrap_err().to_string();
        assert!(err.contains("exhausted"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn filler_produces_the_requested_number_of_lines() {
        let tool = StubTool::new(&spec(
            r#"[{"type":"filler","header":"LOG","line":"entry","lines":50}]"#,
        ));
        let out = tool.execute(json!({})).await.unwrap();
        assert_eq!(out["lines"], 50);
        assert_eq!(out["content"].as_str().unwrap().lines().count(), 51);
    }

    #[test]
    fn error_kinds_map_onto_the_runner_s_own_classes() {
        // The agent loop branches on these, so a silent mismatch would make
        // a "timeout" scenario secretly test the internal-error path.
        assert_eq!(
            ToolErrorKind::from(StubErrorKind::Timeout),
            ToolErrorKind::Timeout
        );
        assert_eq!(
            ToolErrorKind::from(StubErrorKind::RateLimited),
            ToolErrorKind::RateLimited
        );
    }
}
