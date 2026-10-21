//! Run-scoped grants: RustyKrab's own tools for a native agent over MCP
//! (`docs/plans/native-mcp-host-tools.md`, rollout step 1).
//!
//! A native run whose brief needs a host tool is minted a grant: a random
//! bearer token bound to one work item, one run and one actor
//! (`worker:<name>`), the tools the run may call, an expiry at the run's
//! wall deadline and a call quota. The gateway's MCP route resolves the
//! token here, and every call runs here, in the daemon, under
//! `WORK_RUN_CONTEXT` bound to the grant's item and actor, so the scope
//! comes from the grant, never from the agent's arguments, and credentials
//! never leave the daemon.
//!
//! Each grant keeps its own record of what was called and how each call
//! ended, so observation is isolated per grant, and with it per item, run
//! and generation: a call is recorded only on the grant it was made on, a
//! record is sealed when its grant is revoked (a late result is dropped,
//! never carried into the next run's record), and a record attests only
//! the run it was minted for.
//!
//! Admission is atomic: the grant must be live (not revoked, not past its
//! deadline), the tool granted (a read-only review's grant carries none),
//! the quota unspent (every attempt counts, whichever connection made it),
//! and no other call in flight. A retry id (the bridge's session and the
//! JSON-RPC id) repeats its first call's answer, or is refused under a
//! different call. A call runs until the grant's deadline or the tool's
//! own cap, whichever is first. Revocation or expiry cancels what is in
//! flight, but cannot undo what it already did, so that call is recorded
//! as ambiguous rather than as done or not done.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use chrono::{DateTime, Utc};
use rustykrab_core::types::ToolSchema;
use rustykrab_core::work::{ArtifactRef, ResultReport, WorkItemId};
use rustykrab_core::{Error, Result, Tool};
use rustykrab_tools::work_backend::{WorkBackend, WorkRunContext, WORK_RUN_CONTEXT};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::watch;
use tokio::time::Instant;
use uuid::Uuid;

use crate::sandbox::tool_cap_secs;

/// The artifact kind of a call a grant observed. Attesting a report drops
/// every artifact of this kind the agent wrote and adds the grant's own.
pub const MCP_CALL: &str = "mcp_call";

/// What every token starts with, so a leaked one can be recognised.
/// Recognising one is not redacting it: whatever could persist a token
/// still has to keep it out.
const TOKEN_PREFIX: &str = "rkmcp_";

/// What a retry of a call that never finished is told.
const UNKNOWN_EFFECT: &str =
    "the call was cancelled before it finished; whether it took effect is unknown";

/// The tools a native agent is granted first: `work_status`, `work_file`
/// and the question tools, over `backend`. A native run reports with its
/// final message, so `result_report` is not among them, and `work_discuss`
/// takes the item it writes to from its arguments, which a grant's scope
/// never does.
pub fn work_tools(backend: Arc<dyn WorkBackend>) -> Vec<Arc<dyn Tool>> {
    let mut tools = rustykrab_tools::work_tools(backend.clone());
    tools.retain(|tool| matches!(tool.name(), "work_status" | "work_file"));
    tools.extend(rustykrab_tools::question_tools(backend));
    tools
}

/// What a grant is minted with.
pub struct GrantSpec {
    pub item: WorkItemId,
    /// The controller's run id.
    pub run: String,
    /// The event actor its calls are made as, `worker:<name>`.
    pub actor: String,
    /// A read-only review: the grant carries no tools, whatever `tools` says.
    pub read_only: bool,
    /// What the run may call, with their real schemas.
    pub tools: Vec<Arc<dyn Tool>>,
    /// The run's wall deadline (and the lease's, when that is sooner).
    pub deadline: Instant,
    /// How many calls the run may attempt.
    pub quota: u32,
}

/// A grant's bearer token. It never prints: [`expose`](Self::expose) is
/// the one way to read it.
#[derive(Clone, PartialEq, Eq)]
pub struct GrantToken(String);

impl GrantToken {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for GrantToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GrantToken(<redacted>)")
    }
}

/// How one call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CallOutcome {
    /// It ran and its result reports no error.
    Ok,
    /// It ran and failed, or its result reports an error.
    Failed,
    /// It was refused before running: outside the grant.
    Denied,
    /// It was cancelled while it ran (revocation, expiry, or the tool's
    /// cap), so whether it took effect is unknown.
    Ambiguous,
}

/// One call a grant observed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CallObservation {
    pub tool: String,
    pub outcome: CallOutcome,
}

/// What a run did with its grant, returned when the grant is revoked.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GrantRecord {
    pub item: WorkItemId,
    pub run: String,
    pub generation: Uuid,
    /// When the bridge's `initialize` and `tools/list` had both reached the
    /// route. Reachability only: any holder of the grant could have sent
    /// them, so it never proves the agent made a call.
    pub bridge_connected: Option<DateTime<Utc>>,
    pub calls: Vec<CallObservation>,
}

impl GrantRecord {
    /// Attest `report`, the result of `run` on `item`: drop every
    /// [`MCP_CALL`] artifact the agent wrote and add one per call this
    /// grant observed. A record attests only the run it was minted for.
    pub fn attest(&self, item: &str, run: &str, report: &mut ResultReport) -> Result<()> {
        if self.item != item || self.run != run {
            return Err(Error::Auth(
                "an MCP grant's calls attest only the run it was minted for".into(),
            ));
        }
        report
            .artifacts
            .retain(|artifact| artifact.kind != MCP_CALL);
        for call in &self.calls {
            report.artifacts.push(ArtifactRef {
                kind: MCP_CALL.into(),
                value: serde_json::to_string(call).expect("serializable observation"),
            });
        }
        Ok(())
    }
}

/// Why a request on a grant was not answered with a tool result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallError {
    /// No live grant has this token: never minted, revoked, or past its
    /// deadline.
    NotLive,
    /// The tool is not one the grant carries.
    NotGranted,
    /// The grant is a read-only review's, which carries no tools.
    ReadOnly,
    /// The run has attempted as many calls as its quota allows.
    QuotaSpent,
    /// Another call on the grant is still in flight.
    Busy,
    /// The retry id was first used for a different call.
    ConflictingRetry,
    /// The grant was revoked or expired while the call ran.
    Cancelled,
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotLive => "this run's RustyKrab grant is not live: it was revoked or expired",
            Self::NotGranted => "this tool is not granted to this run",
            Self::ReadOnly => "a read-only review is granted no RustyKrab tools",
            Self::QuotaSpent => "this run has used all of its RustyKrab tool calls",
            Self::Busy => {
                "another RustyKrab tool call of this run is still running; one runs at a time"
            }
            Self::ConflictingRetry => "this request id was already used for a different call",
            Self::Cancelled => {
                "the run's grant was revoked or expired during the call; \
                 whether it took effect is unknown"
            }
        })
    }
}

/// Every live grant, by token. One per daemon, shared by whoever mints
/// grants and the gateway's MCP route.
#[derive(Default)]
pub struct McpGrants {
    live: Mutex<HashMap<String, Arc<Live>>>,
}

impl McpGrants {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint a grant. It stays live until it is finished or dropped, or its
    /// deadline passes.
    pub fn mint(self: &Arc<Self>, spec: GrantSpec) -> McpGrant {
        let token = GrantToken(format!(
            "{TOKEN_PREFIX}{}{}",
            Uuid::new_v4().simple(),
            Uuid::new_v4().simple()
        ));
        let (revoked, _) = watch::channel(false);
        let live = Arc::new(Live {
            item: spec.item,
            run: spec.run,
            actor: spec.actor,
            generation: Uuid::new_v4(),
            read_only: spec.read_only,
            tools: if spec.read_only {
                Vec::new()
            } else {
                spec.tools
            },
            deadline: spec.deadline,
            quota: spec.quota,
            revoked,
            state: Mutex::new(State::default()),
        });
        self.map().insert(token.0.clone(), live.clone());
        McpGrant {
            grants: self.clone(),
            token,
            live,
        }
    }

    /// Whether `token` is a live grant's.
    pub fn is_live(&self, token: &str) -> bool {
        self.live_grant(token).is_ok()
    }

    /// The bridge's `initialize` reached the route.
    pub fn initialize(&self, token: &str) -> std::result::Result<(), CallError> {
        self.live_grant(token)?.handshake(false);
        Ok(())
    }

    /// The granted tools' schemas (`tools/list`).
    pub fn list(&self, token: &str) -> std::result::Result<Vec<ToolSchema>, CallError> {
        let live = self.live_grant(token)?;
        live.handshake(true);
        Ok(live
            .tools
            .iter()
            .filter(|tool| tool.available())
            .map(|tool| tool.schema())
            .collect())
    }

    /// Call `name` on the grant (`tools/call`). `retry` identifies the
    /// request across retries of it. The outer error is a refusal or a
    /// cancellation; the inner one is the tool's own failure.
    pub async fn call(
        &self,
        token: &str,
        retry: &str,
        name: &str,
        args: Value,
    ) -> std::result::Result<std::result::Result<Value, String>, CallError> {
        let live = self.live_grant(token)?;
        let (tool, mut revoked) = match live.admit(retry, name, &args)? {
            Admission::Answered(answer) => return Ok(answer),
            Admission::Run(tool, revoked) => (tool, revoked),
        };
        let call = InFlight {
            live: &live,
            key: retry.to_string(),
            tool: name.to_string(),
            settled: false,
        };
        let cap = Duration::from_secs(tool_cap_secs(name, &tool.sandbox_requirements()));
        let until = live.deadline.min(Instant::now() + cap);
        let binding = WorkRunContext {
            item: live.item.clone(),
            actor: live.actor.clone(),
        };
        let execute = WORK_RUN_CONTEXT.scope(binding, tool.execute(args));
        let finished = tokio::select! {
            biased;
            _ = revoked.wait_for(|revoked| *revoked) => None,
            finished = tokio::time::timeout_at(until, execute) => finished.ok(),
        };
        let Some(result) = finished else {
            // Dropped where it was: what it did is unknown, and recorded so.
            drop(call);
            return if live.is_live() {
                Ok(Err(format!(
                    "`{name}` ran past its {} s cap and was stopped; whether it took effect is unknown",
                    cap.as_secs()
                )))
            } else {
                Err(CallError::Cancelled)
            };
        };
        let answer = result.map_err(|e| e.to_string());
        if call.settle(Some(answer.clone())) {
            Ok(answer)
        } else {
            // Revoked as it finished: the record was sealed without it.
            Err(CallError::Cancelled)
        }
    }

    fn live_grant(&self, token: &str) -> std::result::Result<Arc<Live>, CallError> {
        self.map()
            .get(token)
            .filter(|live| live.is_live())
            .cloned()
            .ok_or(CallError::NotLive)
    }

    fn map(&self) -> MutexGuard<'_, HashMap<String, Arc<Live>>> {
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A live grant, held by whoever minted it. Finishing or dropping it
/// revokes it, so the grant ends however the run does.
pub struct McpGrant {
    grants: Arc<McpGrants>,
    token: GrantToken,
    live: Arc<Live>,
}

impl McpGrant {
    pub fn token(&self) -> &GrantToken {
        &self.token
    }

    /// Revoke the grant and return what its run did with it.
    pub fn finish(self) -> GrantRecord {
        self.revoke()
    }

    fn revoke(&self) -> GrantRecord {
        self.grants.map().remove(self.token.expose());
        self.live.seal()
    }
}

impl Drop for McpGrant {
    fn drop(&mut self) {
        self.revoke();
    }
}

struct Live {
    item: WorkItemId,
    run: String,
    actor: String,
    generation: Uuid,
    read_only: bool,
    tools: Vec<Arc<dyn Tool>>,
    deadline: Instant,
    quota: u32,
    revoked: watch::Sender<bool>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    sealed: bool,
    attempts: u32,
    /// The retry id and tool of the call in flight.
    in_flight: Option<(String, String)>,
    retries: HashMap<String, Retry>,
    calls: Vec<CallObservation>,
    initialized: bool,
    listed: bool,
    connected: Option<DateTime<Utc>>,
}

/// A call under one retry id, and its answer once it has one.
struct Retry {
    tool: String,
    args: Value,
    answer: Option<std::result::Result<Value, String>>,
}

enum Admission {
    /// A retry of a call that already has its answer.
    Answered(std::result::Result<Value, String>),
    Run(Arc<dyn Tool>, watch::Receiver<bool>),
}

impl Live {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn is_live(&self) -> bool {
        !self.lock().sealed && Instant::now() < self.deadline
    }

    fn handshake(&self, list: bool) {
        let mut state = self.lock();
        if list {
            state.listed = true;
        } else {
            state.initialized = true;
        }
        if state.initialized && state.listed && state.connected.is_none() && !state.sealed {
            state.connected = Some(Utc::now());
            tracing::info!(item = %self.item, run = %self.run, "a native agent's MCP bridge connected");
        }
    }

    fn admit(
        &self,
        retry: &str,
        name: &str,
        args: &Value,
    ) -> std::result::Result<Admission, CallError> {
        let mut state = self.lock();
        if state.sealed || Instant::now() >= self.deadline {
            return Err(CallError::NotLive);
        }
        if let Some(earlier) = state.retries.get(retry) {
            if earlier.tool != name || earlier.args != *args {
                return Err(CallError::ConflictingRetry);
            }
            return match &earlier.answer {
                Some(answer) => Ok(Admission::Answered(answer.clone())),
                None => Err(CallError::Busy),
            };
        }
        if state.attempts >= self.quota {
            return Err(CallError::QuotaSpent);
        }
        state.attempts += 1;
        let Some(tool) = self
            .tools
            .iter()
            .find(|tool| tool.name() == name && tool.available())
        else {
            state.calls.push(CallObservation {
                // The name is the agent's: keep only enough to read it.
                tool: name.chars().take(80).collect(),
                outcome: CallOutcome::Denied,
            });
            return Err(if self.read_only {
                CallError::ReadOnly
            } else {
                CallError::NotGranted
            });
        };
        if state.in_flight.is_some() {
            return Err(CallError::Busy);
        }
        state.in_flight = Some((retry.to_string(), name.to_string()));
        state.retries.insert(
            retry.to_string(),
            Retry {
                tool: name.to_string(),
                args: args.clone(),
                answer: None,
            },
        );
        Ok(Admission::Run(tool.clone(), self.revoked.subscribe()))
    }

    /// Record how the call under `retry` ended (`None`: it did not), unless
    /// the grant was sealed first. Says whether it was recorded.
    fn settle(
        &self,
        retry: &str,
        tool: &str,
        answer: Option<std::result::Result<Value, String>>,
    ) -> bool {
        let mut state = self.lock();
        if state
            .in_flight
            .as_ref()
            .is_some_and(|(key, _)| key == retry)
        {
            state.in_flight = None;
        }
        if state.sealed {
            return false;
        }
        let outcome = match &answer {
            None => CallOutcome::Ambiguous,
            Some(Ok(value)) if succeeded(value) => CallOutcome::Ok,
            Some(_) => CallOutcome::Failed,
        };
        state.calls.push(CallObservation {
            tool: tool.to_string(),
            outcome,
        });
        if let Some(earlier) = state.retries.get_mut(retry) {
            earlier.answer = Some(answer.unwrap_or_else(|| Err(UNKNOWN_EFFECT.into())));
        }
        true
    }

    /// Seal the record, a call still in flight recorded as ambiguous, and
    /// cancel that call.
    fn seal(&self) -> GrantRecord {
        let mut state = self.lock();
        if !state.sealed {
            state.sealed = true;
            if let Some((_, tool)) = state.in_flight.take() {
                state.calls.push(CallObservation {
                    tool,
                    outcome: CallOutcome::Ambiguous,
                });
            }
        }
        let record = GrantRecord {
            item: self.item.clone(),
            run: self.run.clone(),
            generation: self.generation,
            bridge_connected: state.connected,
            calls: state.calls.clone(),
        };
        drop(state);
        self.revoked.send_replace(true);
        record
    }
}

/// A call admitted and not yet settled. Dropped unsettled (cancelled,
/// timed out, or its request abandoned), it is recorded as ambiguous.
struct InFlight<'a> {
    live: &'a Live,
    key: String,
    tool: String,
    settled: bool,
}

impl InFlight<'_> {
    fn settle(mut self, answer: Option<std::result::Result<Value, String>>) -> bool {
        self.settled = true;
        self.live.settle(&self.key, &self.tool, answer)
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        if !self.settled {
            self.live.settle(&self.key, &self.tool, None);
        }
    }
}

/// Whether a tool's result reports success, read the way the host half's
/// integration observation reads it.
fn succeeded(value: &Value) -> bool {
    value.get("error").is_none_or(Value::is_null) && value["is_error"] != Value::Bool(true)
}

#[cfg(test)]
mod tests;
