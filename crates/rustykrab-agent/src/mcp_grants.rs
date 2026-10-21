//! Run-scoped grants: RustyKrab's own tools for a native agent over MCP
//! (`docs/plans/native-mcp-host-tools.md`, rollout step 1).
//!
//! A native run whose brief needs a host tool is minted a grant: a random
//! bearer token bound to one work item, the run's conversation and one
//! actor (`worker:<name>`), the tools the run may call, an expiry at the
//! run's wall deadline and a call quota. The gateway's MCP route resolves
//! the token here, and every call runs here, in the daemon, under
//! `WORK_RUN_CONTEXT` and `SESSION_TOOL_CONTEXT` bound to the grant as the
//! host half binds them for a local run, so the scope comes from the grant,
//! never from the agent's arguments, and credentials never leave the
//! daemon.
//!
//! Each grant keeps its own record of what was called and how each call
//! ended, so observation is isolated per grant, and with it per item, run
//! and generation: a call is recorded only on the grant it was made on, a
//! record is sealed when its grant is revoked (a late result is dropped,
//! never carried into the next run's record), and only the grant itself
//! attests a report, when its minter finishes it.
//!
//! Admission is atomic: the grant must be live (not revoked, not past its
//! deadline) and its run not ended, the quota unspent (every attempt
//! counts, whichever connection made it), the request id new (the bridge's
//! session and the JSON-RPC id; a reused one is refused, never replayed),
//! the tool granted (a read-only review's grant carries none), and no other
//! call in flight. A call runs until the grant's deadline or the tool's own
//! cap, whichever is first. Revocation or expiry cancels what is in flight,
//! but cannot undo what it already did, so that call is recorded as
//! ambiguous rather than as done or not done. A call that ends a worker run
//! on the host half (a question that parked the item) ends this run too:
//! no call is admitted after it, and the minter is told.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use chrono::{DateTime, Utc};
use rustykrab_core::active_tools::{ActiveToolsRegistry, SessionToolContext, SESSION_TOOL_CONTEXT};
use rustykrab_core::capability::CapabilitySet;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::work::{ArtifactRef, ResultReport, WorkItemId};
use rustykrab_core::Tool;
use rustykrab_tools::work_backend::{WorkBackend, WorkRunContext, WORK_RUN_CONTEXT};
use serde::Serialize;
use serde_json::{json, Value};
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

/// What a call that ended the run tells the agent, beside its result.
const RUN_ENDED: &str = "This run has ended: the item waits for the answer. Call no more tools; \
                         write your final report now and stop.";

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
    /// The run's conversation, which the work tools file and route under.
    pub conversation: Uuid,
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
    /// It ran and its result reports no error or refusal.
    Ok,
    /// It ran and failed, or its result reports an error or a refusal.
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

/// What a run did with its grant, returned when the grant is finished.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GrantRecord {
    /// When the bridge's `initialize` and `tools/list` had both reached the
    /// route. Reachability only: any holder of the grant could have sent
    /// them, so it never proves the agent made a call.
    pub bridge_connected: Option<DateTime<Utc>>,
    /// The summary of the call that ended the run, if one did.
    pub ended: Option<String>,
    pub calls: Vec<CallObservation>,
}

/// Why a request on a grant was not answered with a tool result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallError {
    /// No live grant has this token: never minted, revoked, or past its
    /// deadline.
    NotLive,
    /// The tool is not one the grant carries (a review's carries none).
    NotGranted,
    /// The run has attempted as many calls as its quota allows.
    QuotaSpent,
    /// Another call on the grant is still in flight.
    Busy,
    /// The request id was already used in this session.
    RepeatedId,
    /// An earlier call ended the run.
    Ended,
    /// The grant was revoked or expired while the call ran.
    Cancelled,
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotLive => "this run's RustyKrab grant is not live: it was revoked or expired",
            Self::NotGranted => "this tool is not granted to this run",
            Self::QuotaSpent => "this run has used all of its RustyKrab tool calls",
            Self::Busy => {
                "another RustyKrab tool call of this run is still running; one runs at a time"
            }
            Self::RepeatedId => "this request id was already used; a call is sent once",
            Self::Ended => RUN_ENDED,
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
        let tools = if spec.read_only {
            Vec::new()
        } else {
            spec.tools
        };
        let live = Arc::new(Live {
            binding: WorkRunContext {
                item: spec.item,
                actor: spec.actor,
            },
            // The run's conversation, and nothing a call could load or grow.
            session: SessionToolContext {
                conversation_id: spec.conversation,
                capabilities: Arc::new(CapabilitySet::none()),
                all_tools: Arc::new(tools.clone()),
                active_tools: Arc::new(ActiveToolsRegistry::new()),
                recall: Arc::default(),
                todos: Arc::default(),
            },
            tools,
            deadline: spec.deadline,
            quota: spec.quota,
            revoked: watch::channel(false).0,
            ended: watch::channel(None).0,
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

    /// Call `name` on the grant (`tools/call`) under request id `id`. The
    /// outer error is a refusal or a cancellation; the inner one is a
    /// failure the agent reads as the tool's result.
    pub async fn call(
        &self,
        token: &str,
        id: &str,
        name: &str,
        args: Value,
    ) -> std::result::Result<std::result::Result<Value, String>, CallError> {
        let live = self.live_grant(token)?;
        let (tool, mut revoked) = live.admit(id, name)?;
        let call = InFlight {
            live: &live,
            tool: name.to_string(),
            settled: false,
        };
        let cap = Duration::from_secs(tool_cap_secs(name, &tool.sandbox_requirements()));
        let until = live.deadline.min(Instant::now() + cap);
        let execute = SESSION_TOOL_CONTEXT.scope(
            live.session.clone(),
            WORK_RUN_CONTEXT.scope(live.binding.clone(), tool.execute(args)),
        );
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
        // One judgement of success, for the record and for the agent.
        let mut ended = None;
        let answer = match result {
            Ok(mut value) => match rustykrab_tools::worker_run_end_summary(name, &value) {
                Some(summary) => {
                    value["next"] = json!(RUN_ENDED);
                    ended = Some(summary);
                    Ok(value)
                }
                None if succeeded(&value) => Ok(value),
                None => Err(value.to_string()),
            },
            Err(e) => Err(e.to_string()),
        };
        let outcome = if answer.is_ok() {
            CallOutcome::Ok
        } else {
            CallOutcome::Failed
        };
        if call.settle(outcome, ended) {
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

    /// Resolves, with its summary, when a call ends the run (a question
    /// that parked the item): the run's agent should be stopped then.
    pub async fn ended(&self) -> String {
        let mut ended = self.live.ended.subscribe();
        let summary = match ended.wait_for(Option::is_some).await {
            Ok(summary) => summary.clone(),
            Err(_) => None,
        };
        match summary {
            Some(summary) => summary,
            None => std::future::pending().await,
        }
    }

    /// Revoke the grant and return what its run did with it, attesting
    /// `report`, the run's own result, when there is one: every
    /// [`MCP_CALL`] artifact the agent wrote is replaced by one per call
    /// this grant observed. Only the grant attests, so no other run's
    /// record can.
    pub fn finish(self, report: Option<&mut ResultReport>) -> GrantRecord {
        let record = self.revoke();
        if let Some(report) = report {
            report
                .artifacts
                .retain(|artifact| artifact.kind != MCP_CALL);
            report
                .artifacts
                .extend(record.calls.iter().map(|call| ArtifactRef {
                    kind: MCP_CALL.into(),
                    value: serde_json::to_string(call).expect("serializable observation"),
                }));
        }
        record
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
    binding: WorkRunContext,
    session: SessionToolContext,
    tools: Vec<Arc<dyn Tool>>,
    deadline: Instant,
    quota: u32,
    revoked: watch::Sender<bool>,
    /// The summary of the call that ended the run.
    ended: watch::Sender<Option<String>>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    sealed: bool,
    attempts: u32,
    /// Every request id used.
    ids: HashSet<String>,
    /// The tool of the call in flight.
    in_flight: Option<String>,
    calls: Vec<CallObservation>,
    initialized: bool,
    listed: bool,
    connected: Option<DateTime<Utc>>,
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
            tracing::info!(item = %self.binding.item, conversation = %self.session.conversation_id,
                "a native agent's MCP bridge connected");
        }
    }

    fn admit(
        &self,
        id: &str,
        name: &str,
    ) -> std::result::Result<(Arc<dyn Tool>, watch::Receiver<bool>), CallError> {
        let mut state = self.lock();
        if state.sealed || Instant::now() >= self.deadline {
            return Err(CallError::NotLive);
        }
        if self.ended.borrow().is_some() {
            return Err(CallError::Ended);
        }
        if state.attempts >= self.quota {
            return Err(CallError::QuotaSpent);
        }
        if !state.ids.insert(id.to_string()) {
            return Err(CallError::RepeatedId);
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
            return Err(CallError::NotGranted);
        };
        if state.in_flight.is_some() {
            return Err(CallError::Busy);
        }
        state.in_flight = Some(name.to_string());
        Ok((tool.clone(), self.revoked.subscribe()))
    }

    /// Record how the call in flight ended, and the run's end if it ended
    /// it, unless the grant was sealed first. Says whether it was recorded.
    fn settle(&self, tool: &str, outcome: CallOutcome, ended: Option<String>) -> bool {
        let mut state = self.lock();
        state.in_flight = None;
        if state.sealed {
            return false;
        }
        state.calls.push(CallObservation {
            tool: tool.to_string(),
            outcome,
        });
        if let Some(summary) = ended {
            tracing::info!(item = %self.binding.item, %summary, "a native agent's MCP call ended its run");
            self.ended.send_replace(Some(summary));
        }
        true
    }

    /// Seal the record, a call still in flight recorded as ambiguous, and
    /// cancel that call.
    fn seal(&self) -> GrantRecord {
        let mut state = self.lock();
        if !state.sealed {
            state.sealed = true;
            if let Some(tool) = state.in_flight.take() {
                state.calls.push(CallObservation {
                    tool,
                    outcome: CallOutcome::Ambiguous,
                });
            }
        }
        let record = GrantRecord {
            bridge_connected: state.connected,
            ended: self.ended.borrow().clone(),
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
    tool: String,
    settled: bool,
}

impl InFlight<'_> {
    fn settle(mut self, outcome: CallOutcome, ended: Option<String>) -> bool {
        self.settled = true;
        self.live.settle(&self.tool, outcome, ended)
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        if !self.settled {
            self.live.settle(&self.tool, CallOutcome::Ambiguous, None);
        }
    }
}

/// Whether a tool's result reports success: no error (the host half's
/// integration observation reads it the same way) and no refusal (the
/// work tools refuse with `outcome: rejected`).
fn succeeded(value: &Value) -> bool {
    value.get("error").is_none_or(Value::is_null)
        && value["is_error"] != Value::Bool(true)
        && value["outcome"] != "rejected"
}

#[cfg(test)]
mod tests;
