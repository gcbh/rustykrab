//! The message a parent owes the user (plan section 6.6): one per parent,
//! never one per child, and a single item is its own parent.
//!
//! What it says: the roll-up (leaves done of total); what is running and on
//! which worker; what is queued and why (a trigger, or the items it waits
//! on); what is held and behind which origin; for each failure the child,
//! the order its ladder reached, its error class and what its plan B did;
//! and whatever is asked of the user. [`render`] is a pure function of the
//! snapshot after the transaction that caused the notice and a few store
//! reads gathered beforehand ([`NoticeData`]).

use std::collections::HashMap;

use rustykrab_core::work::{ArtifactRef, EdgeKind, Status, Trigger, WorkItem, WorkItemId};

use crate::graph::Snapshot;
use crate::ladder::{summary, LadderState};

/// Why a notice is owed. Closes and expiries are read off the transaction;
/// the rest are asked for explicitly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Cause {
    /// The root closed.
    Closed(WorkItemId),
    /// An item in the tree expired.
    Expired(WorkItemId),
    /// Something needs the user: a surfaced ladder, a parked question.
    Asked { item: WorkItemId, text: String },
    /// A filing held items for approval (6.1).
    Approval {
        held: Vec<WorkItemId>,
        triggers: Vec<String>,
    },
}

impl Cause {
    /// The item that caused the notice, for the outbox row's `origin`.
    pub fn item(&self) -> Option<&str> {
        match self {
            Cause::Closed(id) | Cause::Expired(id) => Some(id),
            Cause::Asked { item, .. } => Some(item),
            Cause::Approval { held, .. } => held.first().map(String::as_str),
        }
    }
}

/// What rendering needs beyond the snapshot, read from the store first.
#[derive(Debug, Default)]
pub(super) struct NoticeData {
    /// The ladder of each failed or blocked item in the tree.
    pub ladders: HashMap<WorkItemId, LadderState>,
    /// The worker running each active leaf.
    pub workers: HashMap<WorkItemId, String>,
    /// Verified evidence refs of each done leaf.
    pub evidence: HashMap<WorkItemId, Vec<ArtifactRef>>,
    /// The one-line result summary of each done leaf.
    pub summaries: HashMap<WorkItemId, String>,
}

/// How many items a list names before it says "and N more".
const LIST_MAX: usize = 6;
/// How many evidence refs a done line shows.
const REFS_MAX: usize = 4;

/// `#` and the first eight characters of an id.
pub(super) fn short(id: &str) -> String {
    format!("#{}", id.chars().take(8).collect::<String>())
}

/// `"title" (#abcd1234)`.
pub(super) fn label(item: &WorkItem) -> String {
    format!("\"{}\" ({})", item.title.trim(), short(&item.id))
}

fn phrase(status: Status) -> String {
    match status {
        Status::Blocked(r) => format!("blocked ({})", r.as_str()),
        Status::Cancelled(r) => format!("cancelled ({})", r.as_str()),
        other => other.name().to_string(),
    }
}

fn clip(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(max) {
        Some((cut, _)) => format!("{}...", &flat[..cut]),
        None => flat,
    }
}

fn list(title: &str, entries: Vec<String>) -> Option<String> {
    if entries.is_empty() {
        return None;
    }
    let extra = entries.len().saturating_sub(LIST_MAX);
    let mut shown: Vec<String> = entries.into_iter().take(LIST_MAX).collect();
    if extra > 0 {
        shown.push(format!("and {extra} more"));
    }
    Some(format!("{title}: {}.", shown.join("; ")))
}

fn refs(refs: &[ArtifactRef]) -> String {
    refs.iter()
        .take(REFS_MAX)
        .map(|r| format!("{}:{}", r.kind, r.value))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The message for `root`.
pub(super) fn render(snap: &Snapshot, root: &str, data: &NoticeData, causes: &[Cause]) -> String {
    let Some(r) = snap.item(root) else {
        return String::new();
    };
    let is_parent = snap.has_children(root);
    let leaves: Vec<&WorkItem> = if is_parent {
        snap.descendants(root)
            .iter()
            .filter_map(|d| snap.item(d))
            .filter(|i| !snap.has_children(&i.id))
            .collect()
    } else {
        vec![r]
    };
    let mut lines: Vec<String> = Vec::new();

    if is_parent {
        let done = leaves.iter().filter(|i| i.status == Status::Done).count();
        lines.push(format!(
            "{}: {}; {} of {} done.",
            label(r),
            phrase(r.status),
            done,
            leaves.len()
        ));
    } else {
        lines.push(format!("{}: {}.", label(r), phrase(r.status)));
    }

    for cause in causes {
        if let Cause::Approval { held, triggers } = cause {
            let names: Vec<String> = held
                .iter()
                .filter_map(|h| snap.item(h))
                .map(label)
                .collect();
            lines.push(format!(
                "Needs your approval ({}): {} held: {}. Reply /approve {} or /reject {}.",
                triggers.join("; "),
                held.len(),
                names.join(", "),
                short(root),
                short(root)
            ));
        }
    }

    // What finished.
    if is_parent {
        let done: Vec<String> = leaves
            .iter()
            .filter(|i| i.status == Status::Done)
            .map(|i| {
                let mut line = label(i);
                if let Some(s) = data.summaries.get(&i.id) {
                    line.push_str(&format!(": {}", clip(s, 120)));
                }
                if let Some(ev) = data.evidence.get(&i.id).filter(|e| !e.is_empty()) {
                    line.push_str(&format!(" [{}]", refs(ev)));
                }
                line
            })
            .collect();
        lines.extend(list("Done", done));
    } else if r.status == Status::Done {
        if let Some(s) = data.summaries.get(&r.id) {
            lines.push(format!("Result: {}.", clip(s, 240)));
        }
        if let Some(ev) = data.evidence.get(&r.id).filter(|e| !e.is_empty()) {
            lines.push(format!("Evidence: {}.", refs(ev)));
        }
    }

    // What is moving.
    let running: Vec<String> = leaves
        .iter()
        .filter(|i| i.status.is_active())
        .map(|i| match data.workers.get(&i.id) {
            Some(w) => format!("{} on {w}", label(i)),
            None => label(i),
        })
        .collect();
    lines.extend(list("Running", running));

    // What waits, and why.
    let queued: Vec<String> = leaves
        .iter()
        .filter(|i| matches!(i.status, Status::Queued | Status::Ready))
        .map(|i| format!("{} {}", label(i), why_waiting(snap, i)))
        .collect();
    lines.extend(list("Queued", queued));

    // What a cascade holds, behind its origin.
    let held: Vec<String> = leaves
        .iter()
        .filter_map(|i| match i.status {
            Status::Blocked(reason) if reason.is_cascade() => Some(format!(
                "{} behind {} ({})",
                label(i),
                i.status_origin
                    .as_deref()
                    .map_or_else(|| "?".to_string(), short),
                reason.as_str()
            )),
            _ => None,
        })
        .collect();
    lines.extend(list("Held", held));

    // Each failure: the child, the order reached, the error, the plan B.
    for i in &leaves {
        let own_block = matches!(i.status, Status::Blocked(reason) if !reason.is_cascade())
            && i.held_by.is_none();
        if i.status == Status::Failed || own_block {
            lines.push(not_done(snap, i, data));
        }
    }

    for cause in causes {
        match cause {
            Cause::Expired(id) if id != root => {
                if let Some(item) = snap.item(id) {
                    lines.push(format!("Expired: {}.", label(item)));
                }
            }
            Cause::Asked { text, .. } => lines.push(format!("Asked: {}", clip(text, 400))),
            _ => {}
        }
    }

    if matches!(r.status, Status::Cancelled(_)) && is_parent {
        let cancelled = leaves
            .iter()
            .filter(|i| matches!(i.status, Status::Cancelled(_)))
            .count();
        let finished: Vec<String> = leaves
            .iter()
            .filter(|i| i.status.is_closed() && !matches!(i.status, Status::Cancelled(_)))
            .map(|i| format!("{} {}", label(i), phrase(i.status)))
            .collect();
        lines.push(format!("Cancelled {cancelled} open items."));
        lines.extend(list("Finished before the cancel", finished));
    }

    if let (Some(at), false) = (r.expires_at, r.status.is_closed()) {
        lines.push(format!("Expires {}.", at.format("%Y-%m-%d %H:%M UTC")));
    }
    lines.join("\n")
}

/// Why a queued item waits: its trigger, its approval, or its upstreams.
fn why_waiting(snap: &Snapshot, item: &WorkItem) -> String {
    if item.status == Status::Ready {
        return "(ready)".to_string();
    }
    if item.held_by.is_some() {
        return "(waiting for approval)".to_string();
    }
    match &item.trigger {
        Trigger::At(at) => return format!("(at {})", at.format("%Y-%m-%d %H:%M UTC")),
        Trigger::OnCredential(name) => return format!("(waiting for the credential {name})"),
        Trigger::OnMcp(server) => return format!("(waiting for the MCP server {server})"),
        Trigger::OnAnswer(q) => return format!("(waiting for an answer to {q})"),
        Trigger::Now => {}
    }
    let upstreams: Vec<String> = snap
        .edges_held_by(&item.id)
        .filter(|e| match e.kind {
            EdgeKind::Blocks => snap.status(&e.depends_on) != Some(Status::Done),
            EdgeKind::WaitsFor | EdgeKind::ConditionalOnFailure => {
                !snap.status(&e.depends_on).is_some_and(|s| s.is_closed())
            }
            _ => false,
        })
        .map(|e| short(&e.depends_on))
        .collect();
    if upstreams.is_empty() {
        "(queued)".to_string()
    } else {
        format!("(waiting on {})", upstreams.join(", "))
    }
}

fn not_done(snap: &Snapshot, item: &WorkItem, data: &NoticeData) -> String {
    let ladder = data.ladders.get(&item.id);
    let order = ladder.map_or("none", |l| l.order_reached());
    let mut line = format!(
        "Not done: {} {}, reached order {}",
        label(item),
        phrase(item.status),
        order
    );
    let error = ladder.and_then(|l| l.history.iter().rev().find_map(|e| e.error.clone()));
    if let Some(e) = error {
        line.push_str(&format!(
            " ({}/{}: {})",
            e.class.as_str(),
            e.subclass.as_str(),
            clip(&e.detail, 160)
        ));
    }
    line.push('.');
    if let Some(l) = ladder.filter(|l| !l.history.is_empty()) {
        line.push(' ');
        line.push_str(&summary(l));
    }
    let plan_b = snap
        .edges_naming(&item.id)
        .find(|e| e.kind == EdgeKind::ConditionalOnFailure)
        .and_then(|e| snap.item(&e.item));
    if let Some(pb) = plan_b {
        line.push_str(&format!(" Plan B {} is {}", label(pb), phrase(pb.status)));
        if let Some(w) = data.workers.get(&pb.id) {
            line.push_str(&format!(" on {w}"));
        }
        line.push('.');
    }
    line
}
