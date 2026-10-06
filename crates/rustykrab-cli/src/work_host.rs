//! The control layer's host side in the daemon
//! (`docs/plans/control-layer-and-worker-fleet.md`): what the composition
//! root adds around the controller that is not the controller's to know.
//!
//! - **Tool stubs** (`RUSTYKRAB_TOOL_STUBS`, the evaluation switch) are
//!   applied once, before the local worker takes its tool list, so the
//!   worker and the daemon run one stubbed registry; the work tools are
//!   registered after the switch, so `replace` mode keeps them (a stub
//!   that scripts one of them wins).
//! - **The busy signal** of plan 12.1: an interactive turn in flight on the
//!   daemon's model holds the controller's local leases
//!   ([`TurnActivity`]).
//! - **Notice delivery** (plan 6.6): each outbox notice goes to the thread
//!   its item came from, resolved at delivery time through the channel
//!   bindings of the item's `origin_conversation_id`; a scheduled firing's
//!   notice goes to its job's delivery target, and a `done` firing's notice
//!   is its job's own delivery; only with neither does a Telegram notice
//!   fall back to the first allowed chat. Where a notice goes changes here,
//!   never what it says.

use std::sync::Arc;

use rustykrab_control::controller::ModelActivity;
use rustykrab_core::activity::ActivityTracker;
use rustykrab_core::work::Status;
use rustykrab_core::Tool;
use rustykrab_store::{ChannelAddress, OutboxRow, Store};
use rustykrab_tools::{MessageBackend, WorkBackend};
use uuid::Uuid;

use crate::task_queue;

// ── tool stubs and the work tools ──────────────────────────────────────

/// The registry after the stub switch, and the stub names its file hides
/// from the active-tools seed.
pub(crate) type StubbedTools = (Vec<Arc<dyn Tool>>, Vec<String>);

/// Apply `RUSTYKRAB_TOOL_STUBS` to the registry, when it is set. Called
/// once, before the local worker takes its list, so the worker runs the
/// same stub instances (and the same scripted call counts) the daemon does.
/// Also returns the stubs the file marks hidden, which the active-tools
/// seed leaves out. Must never be set on a real deployment.
pub(crate) fn apply_tool_stubs(tools: Vec<Arc<dyn Tool>>) -> anyhow::Result<StubbedTools> {
    let Some(path) = std::env::var_os("RUSTYKRAB_TOOL_STUBS") else {
        return Ok((tools, Vec::new()));
    };
    let path = std::path::PathBuf::from(path);
    let stubs = rustykrab_tools::StubFile::from_path(&path)?;
    let stubbed = stubs.apply(tools);
    tracing::warn!(
        path = %path.display(),
        mode = ?stubs.mode,
        tools = ?stubbed.iter().map(|t| t.name()).collect::<Vec<_>>(),
        "RUSTYKRAB_TOOL_STUBS is set: the tool registry has been replaced with \
         scripted stubs. This is the evaluation harness switch."
    );
    Ok((stubbed, stubs.hidden_names()))
}

/// Register the work tools over `backend`, after the stub switch, so a
/// `replace` stub file keeps them. A name the registry already has (a stub
/// scripting one of them) is left as it is.
///
/// Returns the names it added. The stub switch's active-tools seed leaves
/// them out: the stub file is the harness's closed world, and a worker run
/// declares the work tools it needs itself, so seeding them into every
/// conversation only spends the model suite's deliberately tight context
/// window on schemas no case uses.
pub(crate) fn add_work_tools(
    tools: &mut Vec<Arc<dyn Tool>>,
    backend: Arc<dyn WorkBackend>,
) -> Vec<String> {
    let mut added = Vec::new();
    for tool in rustykrab_tools::work_tools(backend) {
        if !tools.iter().any(|t| t.name() == tool.name()) {
            added.push(tool.name().to_string());
            tools.push(tool);
        }
    }
    added
}

// ── the busy signal (plan 12.1) ────────────────────────────────────────

/// Interactive work in flight on the daemon's one model provider: every
/// turn the runtime runs holds a run guard on the activity tracker (a
/// channel or web turn, a credential wake, a delegated task), and the
/// local worker's own runs do not. While any is in flight the controller
/// leases no local worker on that model.
pub(crate) struct TurnActivity {
    tracker: ActivityTracker,
    model: String,
}

impl ModelActivity for TurnActivity {
    fn busy(&self, model: &str) -> bool {
        model == self.model && self.tracker.runs_in_flight() > 0
    }
}

pub(crate) fn turn_activity(tracker: ActivityTracker, model: &str) -> Arc<dyn ModelActivity> {
    Arc::new(TurnActivity {
        tracker,
        model: model.to_string(),
    })
}

// ── notice delivery (plan 6.6) ─────────────────────────────────────────

/// Where one notice goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Route {
    /// Send it: channel, chat and thread.
    Send {
        channel: String,
        chat: Option<String>,
        thread: Option<String>,
    },
    /// Already delivered another way: a `done` scheduled firing's result
    /// goes out as its job's own message (`scheduled_work.rs`).
    Consumed,
}

fn send(channel: &str, chat: Option<String>, thread: Option<String>) -> Route {
    Route::Send {
        channel: channel.to_string(),
        chat,
        thread,
    }
}

/// The thread a channel address names, as the message backend takes it.
fn to_address(address: ChannelAddress) -> Route {
    match address {
        ChannelAddress::Telegram { chat_id, thread_id } => send(
            "telegram",
            Some(chat_id.to_string()),
            (thread_id != 0).then(|| thread_id.to_string()),
        ),
        ChannelAddress::Slack {
            channel_id,
            thread_ts,
            ..
        } => send(
            "slack",
            Some(channel_id),
            (!thread_ts.is_empty()).then_some(thread_ts),
        ),
        ChannelAddress::Signal { peer } => send("signal", Some(peer), None),
    }
}

/// Resolve where `row` goes, at delivery time: a scheduled firing to its
/// job's target (a `done` one is consumed); else the thread its parent
/// item's `origin_conversation_id` is bound to; else, for a Telegram
/// notice, the first allowed chat; else the row's channel with no address.
pub(crate) async fn route(store: &Store, row: &OutboxRow, default_chat: Option<&str>) -> Route {
    let parent = store.work_get(&row.parent).await.ok().flatten();

    if let Ok(Some(job)) = store.jobs().job_for_work_item(&row.parent).await {
        if parent.as_ref().is_some_and(|p| p.status == Status::Done) {
            return Route::Consumed;
        }
        let conv_id = job
            .conversation_id
            .as_deref()
            .and_then(|c| Uuid::parse_str(c).ok());
        let conv = match conv_id {
            Some(id) => store.conversations().get(id).await.ok(),
            None => None,
        };
        if let Some(conv) = conv {
            let (channel, chat, thread) = task_queue::resolve_delivery_target(
                job.channel.as_deref(),
                job.chat_id.as_deref(),
                job.thread_id.as_deref(),
                &conv,
            );
            if let (Some(channel), Some(chat)) = (channel, chat) {
                return send(&channel, Some(chat), thread);
            }
        }
    }

    let origin = parent
        .as_ref()
        .and_then(|p| p.origin_conversation_id.as_deref())
        .and_then(|c| Uuid::parse_str(c).ok());
    if let Some(conv) = origin {
        match store.channel_bindings().address_of(conv).await {
            Ok(Some(address)) => return to_address(address),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(error = %e, conversation = %conv, "notice's thread not resolved")
            }
        }
    }

    let chat = (row.channel == "telegram")
        .then(|| default_chat.map(str::to_string))
        .flatten();
    send(&row.channel, chat, None)
}

/// One pass over the pending notices: each is routed and sent, and marked
/// delivered only once the send succeeds, so a channel outage delays a
/// notice rather than losing it.
pub(crate) async fn deliver_pending(
    store: &Store,
    backend: &dyn MessageBackend,
    default_chat: Option<&str>,
) {
    let pending = match store.work_outbox_pending().await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, "could not read the work outbox");
            return;
        }
    };
    for row in pending {
        let delivered = match route(store, &row, default_chat).await {
            Route::Consumed => true,
            Route::Send {
                channel,
                chat,
                thread,
            } => match backend
                .send_message(&channel, &row.body, chat.as_deref(), thread.as_deref())
                .await
            {
                Ok(_) => true,
                Err(e) => {
                    tracing::debug!(error = %e, id = %row.id, channel = %channel, "work notice not delivered yet");
                    false
                }
            },
        };
        if delivered {
            if let Err(e) = store.work_outbox_mark_delivered(&row.id).await {
                tracing::warn!(error = %e, id = %row.id, "notice sent but not marked delivered");
            }
        }
    }
}

/// Deliver the controller's notices from the work outbox every
/// `every_secs` (plan section 6.6).
pub(crate) async fn deliver_work_notices(
    store: Store,
    backend: Arc<dyn MessageBackend>,
    default_chat: Option<String>,
    every_secs: u64,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(every_secs.max(1)));
    loop {
        interval.tick().await;
        deliver_pending(&store, backend.as_ref(), default_chat.as_deref()).await;
    }
}

#[cfg(test)]
mod tests;
