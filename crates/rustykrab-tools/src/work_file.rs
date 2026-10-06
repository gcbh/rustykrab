//! `work_file`: file ONE discovered work item (plan sections 6.1 and 14).
//!
//! The model sends draft fields only; the host checks them, fills
//! provenance, and hands the draft to the [`WorkBackend`], whose controller
//! accepts it whole or rejects it whole. Every failed check comes back at
//! once, so a small model can fix the draft in one more call.
//!
//! The draft parser here is shared with `result_report`, whose
//! `discovered` drafts take the same fields (plus a `tmp` name) and the
//! same structural checks.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rustykrab_core::active_tools::with_session_context;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::work::{
    ArtifactRef, DraftEdge, EdgeKind, FailedCheck, ItemRef, PlanOutcome, RejectionReason, Trigger,
    WorkItemDraft, WorkKind, WorkerKind,
};
use rustykrab_core::{validate_tool_args, Error, Result, Tool, ToolError};
use serde_json::{json, Map, Value};

use crate::work_backend::{host_provenance, ToolState, WorkBackend};

// ── bounds (plan section 12: short, typed, pointers) ─────────────────────

pub(crate) const TITLE_MAX: usize = 120;
pub(crate) const OBJECTIVE_MAX: usize = 800;
pub(crate) const DONE_WHEN_MAX: usize = 400;
/// One constraint, decision, check, limit or need.
pub(crate) const ENTRY_MAX: usize = 300;
/// Entries in any one list field.
pub(crate) const LIST_MAX: usize = 12;
/// A tool, MCP server, tmp or class name.
pub(crate) const NAME_MAX: usize = 64;
/// A pointer: a path, URL, message id, commit or item id.
pub(crate) const POINTER_MAX: usize = 500;
pub(crate) const PRIORITY_MIN: i64 = -100;
pub(crate) const PRIORITY_MAX: i64 = 100;

const WORK_KINDS: [&str; 4] = ["personal", "research", "capability", "internal"];
const WORKER_KINDS: [&str; 5] = ["any", "local", "peer", "claude_code", "codex"];
pub(crate) const ARTIFACT_KINDS: [&str; 7] = [
    "message", "path", "url", "commit", "item", "project", "other",
];
const EDGE_KINDS: [&str; 3] = ["blocks", "waits_for", "conditional_on_failure"];
const TRIGGER_KINDS: [&str; 5] = ["now", "at", "on_credential", "on_mcp", "on_answer"];

/// Fields a draft may carry. `tmp` is added for drafts inside a result.
const DRAFT_KEYS: [&str; 19] = [
    "title",
    "objective",
    "done_when",
    "kind",
    "constraints",
    "decisions_made",
    "artifact_refs",
    "required_tools",
    "required_mcp_servers",
    "worker_kind",
    "writable_resources",
    "parent",
    "inputs_from",
    "edges",
    "supersedes",
    "trigger",
    "expires_at",
    "priority",
    "plan",
];

// ── problems ────────────────────────────────────────────────────────────

/// One failed check on model arguments, with the typed reason it maps to.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Problem {
    pub reason: RejectionReason,
    pub detail: String,
}

impl Problem {
    pub(crate) fn invalid(path: &str, field: &str, msg: impl std::fmt::Display) -> Self {
        Problem {
            reason: RejectionReason::InvalidItem,
            detail: format!("{path}{field}: {msg}"),
        }
    }

    /// `reason: detail`, the one-line form a small model reads.
    pub(crate) fn line(&self) -> String {
        format!("{}: {}", self.reason.as_str(), self.detail)
    }
}

/// Why a key a model sent is not one it may set, when the host owns it.
fn host_owned(key: &str) -> Option<&'static str> {
    match key {
        "status" | "reason" | "status_origin" => Some("the controller sets status"),
        "id" => Some("the host assigns ids; the result gives you the new id"),
        "budget" => Some("the controller sets budgets"),
        "conversation_id"
        | "origin_conversation_id"
        | "filed_by_item"
        | "actor"
        | "provenance"
        | "principal" => Some("the host fills provenance"),
        "preconditions" => Some("preconditions are host checks"),
        "plan_id" | "held_by" | "created_at" | "updated_at" | "closed_at" | "lease" => {
            Some("control columns belong to the controller")
        }
        "fingerprint" | "observed_by" => Some("the controller classifies errors"),
        _ => None,
    }
}

/// Flag every key outside `allowed`, explaining the host-owned ones.
pub(crate) fn check_keys(
    obj: &Map<String, Value>,
    allowed: &[&str],
    path: &str,
    out: &mut Vec<Problem>,
) {
    for key in obj.keys() {
        if allowed.contains(&key.as_str()) {
            continue;
        }
        let why = match host_owned(key) {
            Some(owner) => format!("not yours to set: {owner}; leave it out"),
            None => format!("unknown field; allowed: {}", allowed.join(", ")),
        };
        out.push(Problem::invalid(path, key, why));
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

// ── field readers ───────────────────────────────────────────────────────

/// A string field, trimmed. `required` rejects a missing or empty value.
pub(crate) fn take_text(
    obj: &Map<String, Value>,
    key: &str,
    path: &str,
    max: usize,
    required: bool,
    out: &mut Vec<Problem>,
) -> String {
    match obj.get(key) {
        None | Some(Value::Null) => {
            if required {
                out.push(Problem::invalid(path, key, "is missing"));
            }
            String::new()
        }
        Some(Value::String(s)) => {
            let t = s.trim();
            if required && t.is_empty() {
                out.push(Problem::invalid(path, key, "is empty"));
            }
            let n = t.chars().count();
            if n > max {
                out.push(Problem::invalid(
                    path,
                    key,
                    format!("is {n} chars; keep it to {max}"),
                ));
            }
            t.to_string()
        }
        Some(other) => {
            out.push(Problem::invalid(
                path,
                key,
                format!("must be a string, got {}", type_name(other)),
            ));
            String::new()
        }
    }
}

/// An optional string field; empty reads as absent.
pub(crate) fn take_opt_text(
    obj: &Map<String, Value>,
    key: &str,
    path: &str,
    max: usize,
    out: &mut Vec<Problem>,
) -> Option<String> {
    let t = take_text(obj, key, path, max, false, out);
    (!t.is_empty()).then_some(t)
}

/// A list of strings: trimmed, deduplicated, each non-empty and bounded.
pub(crate) fn take_text_list(
    obj: &Map<String, Value>,
    key: &str,
    path: &str,
    max_items: usize,
    max_len: usize,
    out: &mut Vec<Problem>,
) -> Vec<String> {
    let arr = match obj.get(key) {
        None | Some(Value::Null) => return Vec::new(),
        Some(Value::Array(a)) => a,
        Some(other) => {
            out.push(Problem::invalid(
                path,
                key,
                format!("must be an array of strings, got {}", type_name(other)),
            ));
            return Vec::new();
        }
    };
    if arr.len() > max_items {
        out.push(Problem::invalid(
            path,
            key,
            format!("has {} entries; keep it to {max_items}", arr.len()),
        ));
    }
    let mut seen = HashSet::new();
    let mut items = Vec::new();
    for (i, v) in arr.iter().enumerate() {
        let field = format!("{key}[{i}]");
        match v {
            Value::String(s) => {
                let t = s.trim();
                if t.is_empty() {
                    out.push(Problem::invalid(path, &field, "is empty"));
                } else if t.chars().count() > max_len {
                    out.push(Problem::invalid(
                        path,
                        &field,
                        format!("is over {max_len} chars; use a pointer, not the content"),
                    ));
                } else if seen.insert(t.to_string()) {
                    items.push(t.to_string());
                }
            }
            other => out.push(Problem::invalid(
                path,
                &field,
                format!("must be a string, got {}", type_name(other)),
            )),
        }
    }
    items
}

/// `[{kind, value}]` pointers.
pub(crate) fn take_artifacts(
    obj: &Map<String, Value>,
    key: &str,
    path: &str,
    max_items: usize,
    out: &mut Vec<Problem>,
) -> Vec<ArtifactRef> {
    take_artifacts_of(obj, key, path, max_items, &ARTIFACT_KINDS, out)
}

/// [`take_artifacts`] with the kinds this field allows.
pub(crate) fn take_artifacts_of(
    obj: &Map<String, Value>,
    key: &str,
    path: &str,
    max_items: usize,
    kinds: &[&str],
    out: &mut Vec<Problem>,
) -> Vec<ArtifactRef> {
    let arr = match obj.get(key) {
        None | Some(Value::Null) => return Vec::new(),
        Some(Value::Array(a)) => a,
        Some(other) => {
            out.push(Problem::invalid(
                path,
                key,
                format!(
                    "must be an array of {{kind, value}}, got {}",
                    type_name(other)
                ),
            ));
            return Vec::new();
        }
    };
    if arr.len() > max_items {
        out.push(Problem::invalid(
            path,
            key,
            format!("has {} entries; keep it to {max_items}", arr.len()),
        ));
    }
    let mut refs = Vec::new();
    for (i, v) in arr.iter().enumerate() {
        let at = format!("{path}{key}[{i}].");
        let Some(entry) = v.as_object() else {
            out.push(Problem::invalid(
                path,
                &format!("{key}[{i}]"),
                "must be an object {kind, value}",
            ));
            continue;
        };
        check_keys(entry, &["kind", "value"], &at, out);
        let kind = take_text(entry, "kind", &at, NAME_MAX, true, out);
        if !kind.is_empty() && !kinds.contains(&kind.as_str()) {
            out.push(Problem::invalid(
                &at,
                "kind",
                format!("`{kind}` is not one of {}", kinds.join(", ")),
            ));
        }
        let value = take_text(entry, "value", &at, POINTER_MAX, true, out);
        if !kind.is_empty() && !value.is_empty() {
            refs.push(ArtifactRef { kind, value });
        }
    }
    refs
}

fn parse_time(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

fn take_trigger(obj: &Map<String, Value>, path: &str, out: &mut Vec<Problem>) -> Trigger {
    let v = match obj.get("trigger") {
        None | Some(Value::Null) => return Trigger::Now,
        Some(Value::String(s)) if s.trim() == "now" => return Trigger::Now,
        Some(Value::Object(m)) => m,
        Some(_) => {
            out.push(Problem::invalid(
                path,
                "trigger",
                "must be {kind, value}, e.g. {\"kind\": \"at\", \"value\": \"2026-10-01T09:00:00Z\"}",
            ));
            return Trigger::Now;
        }
    };
    let at = format!("{path}trigger.");
    check_keys(v, &["kind", "value"], &at, out);
    let kind = take_text(v, "kind", &at, NAME_MAX, true, out);
    let value = take_text(v, "value", &at, POINTER_MAX, false, out);
    let need_value = |out: &mut Vec<Problem>| {
        if value.is_empty() {
            out.push(Problem::invalid(
                &at,
                "value",
                format!("is required for `{kind}`"),
            ));
        }
    };
    match kind.as_str() {
        "" | "now" => Trigger::Now,
        "at" => match parse_time(&value) {
            Some(t) => Trigger::At(t),
            None => {
                out.push(Problem::invalid(
                    &at,
                    "value",
                    "must be an RFC 3339 time, e.g. 2026-10-01T09:00:00Z",
                ));
                Trigger::Now
            }
        },
        "on_credential" => {
            need_value(out);
            Trigger::OnCredential(value.clone())
        }
        "on_mcp" => {
            need_value(out);
            Trigger::OnMcp(value.clone())
        }
        "on_answer" => {
            need_value(out);
            Trigger::OnAnswer(value.clone())
        }
        other => {
            out.push(Problem::invalid(
                &at,
                "kind",
                format!("`{other}` is not one of {}", TRIGGER_KINDS.join(", ")),
            ));
            Trigger::Now
        }
    }
}

/// How a draft may name other items.
#[derive(Clone, Copy)]
pub(crate) enum DraftMode<'a> {
    /// `work_file`: one draft, existing items by id only.
    File,
    /// A `discovered` draft in a result: existing items by id, sibling
    /// drafts by the `tmp` names declared in the same report.
    Discovered { tmps: &'a HashSet<String> },
}

/// Read one reference: an item id, or in a result a sibling's `tmp`.
fn parse_ref(
    v: &Value,
    mode: DraftMode<'_>,
    path: &str,
    field: &str,
    out: &mut Vec<Problem>,
) -> Option<ItemRef> {
    let tmps = match mode {
        DraftMode::File => None,
        DraftMode::Discovered { tmps } => Some(tmps),
    };
    match v {
        Value::String(s) => {
            let id = s.trim().trim_start_matches('#').trim();
            if id.is_empty() {
                out.push(Problem::invalid(path, field, "is empty"));
                return None;
            }
            if id.chars().count() > POINTER_MAX {
                out.push(Problem::invalid(path, field, "is not an item id"));
                return None;
            }
            if tmps.is_some_and(|t| t.contains(id)) {
                return Some(ItemRef::Tmp {
                    tmp: id.to_string(),
                });
            }
            Some(ItemRef::Id(id.to_string()))
        }
        Value::Object(m) if tmps.is_some() => {
            let tmp = m.get("tmp").and_then(Value::as_str).map(str::trim);
            match tmp {
                Some(t) if m.len() == 1 && tmps.is_some_and(|all| all.contains(t)) => {
                    Some(ItemRef::Tmp { tmp: t.to_string() })
                }
                Some(t) if m.len() == 1 => {
                    out.push(Problem {
                        reason: RejectionReason::UnknownRef,
                        detail: format!("{path}{field}: no draft in this report has tmp `{t}`"),
                    });
                    None
                }
                _ => {
                    out.push(Problem::invalid(
                        path,
                        field,
                        "must be an item id or {\"tmp\": name}",
                    ));
                    None
                }
            }
        }
        _ => {
            out.push(Problem::invalid(path, field, "must be an item id string"));
            None
        }
    }
}

fn take_refs(
    obj: &Map<String, Value>,
    key: &str,
    mode: DraftMode<'_>,
    path: &str,
    out: &mut Vec<Problem>,
) -> Vec<ItemRef> {
    let arr = match obj.get(key) {
        None | Some(Value::Null) => return Vec::new(),
        Some(Value::Array(a)) => a,
        Some(other) => {
            out.push(Problem::invalid(
                path,
                key,
                format!("must be an array of item ids, got {}", type_name(other)),
            ));
            return Vec::new();
        }
    };
    if arr.len() > LIST_MAX {
        out.push(Problem::invalid(
            path,
            key,
            format!("has {} entries; keep it to {LIST_MAX}", arr.len()),
        ));
    }
    let mut refs: Vec<ItemRef> = Vec::new();
    for (i, v) in arr.iter().enumerate() {
        if let Some(r) = parse_ref(v, mode, path, &format!("{key}[{i}]"), out) {
            if !refs.contains(&r) {
                refs.push(r);
            }
        }
    }
    refs
}

/// Parse and check one draft. Problems are appended to `out`; the draft
/// returned is only meaningful when none were added.
///
/// `path` prefixes every problem (`""` for `work_file`, `discovered[2].`
/// inside a result).
pub(crate) fn parse_draft(
    value: &Value,
    mode: DraftMode<'_>,
    path: &str,
    out: &mut Vec<Problem>,
) -> WorkItemDraft {
    let Some(obj) = value.as_object() else {
        out.push(Problem {
            reason: RejectionReason::InvalidItem,
            detail: format!("{path}must be an object with title, objective and done_when"),
        });
        return WorkItemDraft::default();
    };

    let mut allowed: Vec<&str> = DRAFT_KEYS.to_vec();
    if matches!(mode, DraftMode::Discovered { .. }) {
        allowed.push("tmp");
    }
    check_keys(obj, &allowed, path, out);

    let tmp = match mode {
        DraftMode::File => None,
        DraftMode::Discovered { .. } => take_opt_text(obj, "tmp", path, NAME_MAX, out),
    };

    let title = take_text(obj, "title", path, TITLE_MAX, true, out);
    if title.contains('\n') {
        out.push(Problem::invalid(path, "title", "must be one line"));
    }
    let objective = take_text(obj, "objective", path, OBJECTIVE_MAX, true, out);
    let done_when = take_text(obj, "done_when", path, DONE_WHEN_MAX, true, out);

    let kind = match take_opt_text(obj, "kind", path, NAME_MAX, out) {
        None => None,
        Some(raw) => match WorkKind::parse(&raw) {
            Some(WorkKind::Code) => {
                out.push(Problem {
                    reason: RejectionReason::KindNotAllowed,
                    detail: format!(
                        "{path}kind: `code` work enters only through the delivery path"
                    ),
                });
                None
            }
            Some(WorkKind::Proposal) => {
                out.push(Problem {
                    reason: RejectionReason::KindNotAllowed,
                    detail: format!("{path}kind: `proposal` items come only from dreaming"),
                });
                None
            }
            Some(k) => Some(k),
            None => {
                out.push(Problem::invalid(
                    path,
                    "kind",
                    format!("`{raw}` is not one of {}", WORK_KINDS.join(", ")),
                ));
                None
            }
        },
    };

    let worker_kind = match take_opt_text(obj, "worker_kind", path, NAME_MAX, out) {
        None => WorkerKind::Any,
        Some(raw) => WorkerKind::parse(&raw).unwrap_or_else(|| {
            out.push(Problem::invalid(
                path,
                "worker_kind",
                format!("`{raw}` is not one of {}", WORKER_KINDS.join(", ")),
            ));
            WorkerKind::Any
        }),
    };

    let constraints = take_text_list(obj, "constraints", path, LIST_MAX, ENTRY_MAX, out);
    let decisions_made = take_text_list(obj, "decisions_made", path, LIST_MAX, ENTRY_MAX, out);
    let artifact_refs = take_artifacts(obj, "artifact_refs", path, LIST_MAX, out);
    let required_tools = take_text_list(obj, "required_tools", path, LIST_MAX, NAME_MAX, out);
    let required_mcp_servers =
        take_text_list(obj, "required_mcp_servers", path, LIST_MAX, NAME_MAX, out);
    let writable_resources =
        take_text_list(obj, "writable_resources", path, LIST_MAX, ENTRY_MAX, out);

    let parent = match obj.get("parent") {
        None | Some(Value::Null) => None,
        // Parents by id only: a draft never nests under a sibling draft.
        Some(v) => parse_ref(v, DraftMode::File, path, "parent", out),
    };
    let inputs_from = take_refs(obj, "inputs_from", mode, path, out);

    // Edges, with `supersedes` edges folded into the one `supersedes` field.
    let mut edges: Vec<DraftEdge> = Vec::new();
    let mut supersedes: Vec<String> = Vec::new();
    match obj.get("edges") {
        None | Some(Value::Null) => {}
        Some(Value::Array(arr)) => {
            if arr.len() > LIST_MAX {
                out.push(Problem::invalid(
                    path,
                    "edges",
                    format!("has {} entries; keep it to {LIST_MAX}", arr.len()),
                ));
            }
            for (i, e) in arr.iter().enumerate() {
                let at = format!("{path}edges[{i}].");
                let Some(m) = e.as_object() else {
                    out.push(Problem::invalid(
                        path,
                        &format!("edges[{i}]"),
                        "must be an object {kind, depends_on}",
                    ));
                    continue;
                };
                check_keys(m, &["kind", "depends_on"], &at, out);
                let raw_kind = take_text(m, "kind", &at, NAME_MAX, true, out);
                let kind = EdgeKind::ALL
                    .iter()
                    .copied()
                    .find(|k| k.as_str() == raw_kind);
                let Some(dep) = m.get("depends_on") else {
                    out.push(Problem::invalid(&at, "depends_on", "is missing"));
                    continue;
                };
                let Some(dep) = parse_ref(dep, mode, &at, "depends_on", out) else {
                    continue;
                };
                match kind {
                    Some(EdgeKind::DiscoveredFrom) => out.push(Problem::invalid(
                        &at,
                        "kind",
                        "`discovered_from` is filled by the host; leave it out",
                    )),
                    Some(EdgeKind::Supersedes) => match dep {
                        ItemRef::Id(id) => supersedes.push(id),
                        ItemRef::Tmp { .. } => out.push(Problem::invalid(
                            &at,
                            "depends_on",
                            "supersedes names an existing item, not a draft",
                        )),
                    },
                    Some(kind) => {
                        let edge = DraftEdge {
                            kind,
                            depends_on: dep,
                        };
                        if !edges.contains(&edge) {
                            edges.push(edge);
                        }
                    }
                    None if raw_kind.is_empty() => {}
                    None => out.push(Problem::invalid(
                        &at,
                        "kind",
                        format!("`{raw_kind}` is not one of {}", EDGE_KINDS.join(", ")),
                    )),
                }
            }
        }
        Some(other) => out.push(Problem::invalid(
            path,
            "edges",
            format!("must be an array, got {}", type_name(other)),
        )),
    }
    match obj.get("supersedes") {
        None | Some(Value::Null) => {}
        Some(Value::String(s)) => {
            let id = s.trim().trim_start_matches('#').trim();
            if id.is_empty() {
                out.push(Problem::invalid(path, "supersedes", "is empty"));
            } else {
                supersedes.push(id.to_string());
            }
        }
        Some(Value::Array(arr)) => {
            for v in arr {
                match v.as_str().map(str::trim) {
                    Some(id) if !id.is_empty() => supersedes.push(id.to_string()),
                    _ => out.push(Problem::invalid(path, "supersedes", "must be an item id")),
                }
            }
        }
        Some(other) => out.push(Problem::invalid(
            path,
            "supersedes",
            format!("must be an item id, got {}", type_name(other)),
        )),
    }
    let mut seen_targets = HashSet::new();
    supersedes.retain(|id| seen_targets.insert(id.clone()));
    if supersedes.len() > 1 {
        out.push(Problem::invalid(
            path,
            "supersedes",
            format!(
                "a draft may name at most one supersedes target; got {}",
                supersedes.join(", ")
            ),
        ));
    }
    let supersedes = supersedes.into_iter().next();
    if let Some(old) = &supersedes {
        let waits_on_it = edges
            .iter()
            .any(|e| matches!(&e.depends_on, ItemRef::Id(id) if id == old));
        if waits_on_it {
            out.push(Problem::invalid(
                path,
                "supersedes",
                format!("cannot both depend on and supersede `{old}`"),
            ));
        }
    }
    if let Some(own) = &tmp {
        let me = ItemRef::Tmp { tmp: own.clone() };
        if edges.iter().any(|e| e.depends_on == me) || inputs_from.contains(&me) {
            out.push(Problem::invalid(
                path,
                "edges",
                "a draft cannot depend on itself",
            ));
        }
    }

    let expires_at = match take_opt_text(obj, "expires_at", path, NAME_MAX, out) {
        None => None,
        Some(raw) => match parse_time(&raw) {
            Some(t) if t <= Utc::now() => {
                out.push(Problem::invalid(path, "expires_at", "is in the past"));
                None
            }
            Some(t) => Some(t),
            None => {
                out.push(Problem::invalid(
                    path,
                    "expires_at",
                    "must be an RFC 3339 time, e.g. 2026-10-01T09:00:00Z",
                ));
                None
            }
        },
    };
    let trigger = take_trigger(obj, path, out);

    let priority = match obj.get("priority") {
        None | Some(Value::Null) => 0,
        Some(v) => match v.as_i64() {
            Some(p) if (PRIORITY_MIN..=PRIORITY_MAX).contains(&p) => p as i32,
            _ => {
                out.push(Problem::invalid(
                    path,
                    "priority",
                    format!("must be an integer from {PRIORITY_MIN} to {PRIORITY_MAX}"),
                ));
                0
            }
        },
    };
    let plan = match obj.get("plan") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(other) => {
            out.push(Problem::invalid(
                path,
                "plan",
                format!("must be true or false, got {}", type_name(other)),
            ));
            false
        }
    };

    WorkItemDraft {
        tmp,
        kind,
        title,
        objective,
        done_when,
        constraints,
        decisions_made,
        artifact_refs,
        required_tools,
        required_mcp_servers,
        worker_kind,
        writable_resources,
        parent,
        inputs_from,
        trigger,
        preconditions: Vec::new(),
        expires_at,
        budget: None,
        priority,
        edges,
        supersedes,
        plan,
        // The review facets are not a model's to set: `work_file` files no
        // proposals, and a capability's mode comes from the ladder or REST.
        ..WorkItemDraft::default()
    }
}

/// The draft fields as JSON-schema properties. `nested` gives the terse
/// form used inside `result_report.discovered`, with a `tmp` name.
pub(crate) fn draft_properties(nested: bool) -> Map<String, Value> {
    let strings =
        |d: &str| json!({ "type": "array", "items": { "type": "string" }, "description": d });
    let dep = if nested {
        "An item id, or the tmp of another draft in this report."
    } else {
        "An existing item id."
    };
    let mut p = Map::new();
    p.insert(
        "title".into(),
        json!({ "type": "string", "description": format!("Short name, one line (max {TITLE_MAX} chars).") }),
    );
    p.insert(
        "objective".into(),
        json!({ "type": "string", "description": format!("What the work must achieve (max {OBJECTIVE_MAX} chars).") }),
    );
    p.insert(
        "done_when".into(),
        json!({ "type": "string", "description": format!("The check that proves it done (max {DONE_WHEN_MAX} chars).") }),
    );
    p.insert(
        "kind".into(),
        json!({ "type": "string", "enum": WORK_KINDS, "description": "Default personal." }),
    );
    p.insert(
        "constraints".into(),
        strings("One explicit constraint per entry."),
    );
    p.insert(
        "decisions_made".into(),
        strings("Choices already made, one per entry."),
    );
    p.insert(
        "artifact_refs".into(),
        json!({
            "type": "array",
            "description": "Pointers, never content. A project ref names the durable project UUID for context handoff.",
            "items": {
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "enum": ARTIFACT_KINDS },
                    "value": { "type": "string" }
                },
                "required": ["kind", "value"],
                "additionalProperties": false
            }
        }),
    );
    p.insert(
        "required_tools".into(),
        strings("Exact tool names the work needs."),
    );
    p.insert(
        "required_mcp_servers".into(),
        strings("MCP servers the work needs."),
    );
    p.insert(
        "worker_kind".into(),
        json!({ "type": "string", "enum": WORKER_KINDS, "description": "Only when a user or policy requires it. Default any." }),
    );
    p.insert(
        "writable_resources".into(),
        strings("What the work writes: a calendar, a mailbox, a repo."),
    );
    p.insert(
        "parent".into(),
        json!({ "type": "string", "description": "Existing item id to file under." }),
    );
    p.insert(
        "inputs_from".into(),
        json!({ "type": "array", "items": { "type": "string" }, "description": format!("Items whose results it needs. Each: {dep}") }),
    );
    p.insert(
        "edges".into(),
        json!({
            "type": "array",
            "description": "{kind: blocks, depends_on: A}: A must be done first. waits_for: A must close. conditional_on_failure: runs only if A fails.",
            "items": {
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "enum": EDGE_KINDS },
                    "depends_on": { "type": "string", "description": dep }
                },
                "required": ["kind", "depends_on"],
                "additionalProperties": false
            }
        }),
    );
    p.insert(
        "supersedes".into(),
        json!({ "type": "string", "description": "The ONE waiting item this replaces." }),
    );
    p.insert(
        "trigger".into(),
        json!({
            "type": "object",
            "description": "When it may start. Default now. at: an RFC 3339 time; on_credential, on_mcp, on_answer: a name.",
            "properties": {
                "kind": { "type": "string", "enum": TRIGGER_KINDS },
                "value": { "type": "string" }
            },
            "required": ["kind"],
            "additionalProperties": false
        }),
    );
    p.insert(
        "expires_at".into(),
        json!({ "type": "string", "description": "RFC 3339 time after which the work is pointless." }),
    );
    p.insert(
        "priority".into(),
        json!({ "type": "integer", "description": format!("{PRIORITY_MIN} to {PRIORITY_MAX}. Default 0.") }),
    );
    p.insert(
        "plan".into(),
        json!({ "type": "boolean", "description": "true only for several deliverables, a wait on the world between steps, or independent parts: a planning step then builds the graph." }),
    );
    if nested {
        p.insert(
            "tmp".into(),
            json!({ "type": "string", "description": "Short local name other drafts in this report may depend on." }),
        );
        // Terse nested schema: the top-level tool description carries the
        // guidance, and every nested description costs context.
        for key in [
            "kind",
            "constraints",
            "decisions_made",
            "required_mcp_servers",
            "worker_kind",
            "writable_resources",
            "expires_at",
            "priority",
        ] {
            if let Some(Value::Object(m)) = p.get_mut(key) {
                m.remove("description");
            }
        }
    }
    p
}

// ── the tool ────────────────────────────────────────────────────────────

/// Whether a required tool is in the caller's reach.
enum Reach {
    Fine,
    LoadIt,
    Unknown,
}

/// Settle a required tool against the runner's session first (what this
/// caller has loaded and may load), then the host registry.
fn reach(backend: &dyn WorkBackend, name: &str) -> Reach {
    let here = with_session_context(|ctx| {
        let registered = ctx
            .all_tools
            .iter()
            .any(|t| t.name() == name && t.available());
        if !registered || !ctx.capabilities.can_use_tool(name) {
            return None;
        }
        // Callable counts: a tool a search appended is as loaded as a
        // declared one (plan section 12).
        Some(ctx.active_tools.is_callable(ctx.conversation_id, name))
    });
    match here {
        Some(Some(true)) => Reach::Fine,
        Some(Some(false)) => Reach::LoadIt,
        // In a session but beyond this caller's reach: filing is right when
        // the host has the tool for some other worker.
        Some(None) => match backend.tool_state(name) {
            ToolState::Unknown => Reach::Unknown,
            ToolState::Loaded | ToolState::RegisteredUnloaded => Reach::Fine,
        },
        None => match backend.tool_state(name) {
            ToolState::Loaded => Reach::Fine,
            ToolState::RegisteredUnloaded => Reach::LoadIt,
            ToolState::Unknown => Reach::Unknown,
        },
    }
}

/// Files one discovered work item through a [`WorkBackend`].
pub struct WorkFileTool {
    backend: Arc<dyn WorkBackend>,
}

impl WorkFileTool {
    pub fn new(backend: Arc<dyn WorkBackend>) -> Self {
        Self { backend }
    }

    /// Host checks that need the backend: required tools and MCP servers.
    /// Returns the unconfigured MCP servers the item will wait on.
    fn check_capabilities(&self, draft: &WorkItemDraft, out: &mut Vec<Problem>) -> Vec<String> {
        for name in &draft.required_tools {
            match reach(self.backend.as_ref(), name) {
                Reach::Fine => {}
                Reach::LoadIt => out.push(Problem::invalid(
                    "",
                    "required_tools",
                    format!(
                        "`{name}` is registered but not loaded: load it with tools_load and \
                         do the work in this run instead of filing it"
                    ),
                )),
                Reach::Unknown => out.push(Problem::invalid(
                    "",
                    "required_tools",
                    format!(
                        "`{name}` is not a registered tool: use an exact name from \
                         tools_list, or leave it out"
                    ),
                )),
            }
        }
        draft
            .required_mcp_servers
            .iter()
            .filter(|s| !self.backend.mcp_server_configured(s))
            .cloned()
            .collect()
    }
}

/// Render a filing's outcome compactly for a small model.
fn render_outcome(outcome: &PlanOutcome, draft: &WorkItemDraft, waiting_mcp: &[String]) -> Value {
    match outcome {
        PlanOutcome::Accepted(a) => {
            let mut out = json!({ "outcome": "accepted", "id": a.root });
            let mut notes = vec![format!("filed as {}", a.root)];
            let others: Vec<&String> = a.ids.values().filter(|id| **id != a.root).collect();
            if !others.is_empty() {
                out["also_filed"] = json!(others);
            }
            if draft.plan {
                notes.push("a planning step will build its graph".into());
            }
            if !waiting_mcp.is_empty() {
                out["waits"] = json!("needs_tool");
                out["needs"] = json!(waiting_mcp
                    .iter()
                    .map(|s| format!("mcp:{s}"))
                    .collect::<Vec<_>>());
                notes.push(format!(
                    "it will wait as needs_tool until MCP server {} is configured",
                    waiting_mcp
                        .iter()
                        .map(|s| format!("`{s}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if !a.held.is_empty() {
                out["held"] = json!(true);
                notes.push("held until the user approves".into());
            }
            if !a.warnings.is_empty() {
                out["warnings"] = json!(a
                    .warnings
                    .iter()
                    .map(|w| {
                        let check = serde_json::to_value(w.check)
                            .ok()
                            .and_then(|v| v.as_str().map(str::to_string))
                            .unwrap_or_default();
                        format!("{check}: {}", w.items.join(" "))
                    })
                    .collect::<Vec<_>>());
            }
            out["note"] = json!(notes.join("; "));
            out
        }
        PlanOutcome::Rejected(r) => rejected(r.failed.iter().map(failed_line).collect()),
    }
}

fn failed_line(f: &FailedCheck) -> String {
    let offending: Vec<String> = f
        .offending
        .iter()
        .map(|r| match r {
            ItemRef::Id(id) => id.clone(),
            ItemRef::Tmp { tmp } => format!("tmp:{tmp}"),
        })
        .collect();
    let mut line = f.reason.as_str().to_string();
    if !offending.is_empty() {
        line.push_str(&format!(" ({})", offending.join(", ")));
    }
    if !f.detail.is_empty() {
        line.push_str(": ");
        line.push_str(&f.detail);
    }
    line
}

fn rejected(failed: Vec<String>) -> Value {
    json!({
        "outcome": "rejected",
        "failed": failed,
        "next": "fix every failed check, then call work_file again",
    })
}

#[async_trait]
impl Tool for WorkFileTool {
    fn name(&self) -> &str {
        "work_file"
    }

    fn description(&self) -> &str {
        "File ONE work item that should not be done in this run: follow-up work you found \
         that needs another time, tool, person or worker. Give title, objective and \
         done_when; constraints, decisions and refs are separate short entries, and refs are \
         pointers, not content. Set plan: true only for several deliverables, a wait on the \
         world between steps, or independent parts. You never set status, ids or budget; \
         the result gives the new id. If a required tool is merely not loaded, load it and \
         do the work yourself."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": draft_properties(false),
                "required": ["title", "objective", "done_when"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let schema = self.schema();
        validate_tool_args(&schema.parameters, &args).map_err(Error::ToolExecution)?;

        let mut problems = Vec::new();
        let draft = parse_draft(&args, DraftMode::File, "", &mut problems);
        let waiting_mcp = self.check_capabilities(&draft, &mut problems);
        if !problems.is_empty() {
            return Ok(rejected(problems.iter().map(Problem::line).collect()));
        }

        let outcome = self
            .backend
            .file(draft.clone(), host_provenance())
            .await
            .map_err(|e| {
                Error::ToolExecution(ToolError::internal(format!("work_file failed: {e}")))
            })?;
        Ok(render_outcome(&outcome, &draft, &waiting_mcp))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_backend::{
        StubWorkBackend, WorkCall, WorkRunContext, DEFAULT_ACTOR, WORK_RUN_CONTEXT,
    };
    use rustykrab_core::active_tools::{
        ActiveToolsRegistry, SessionToolContext, SESSION_TOOL_CONTEXT,
    };
    use rustykrab_core::capability::CapabilitySet;
    use rustykrab_core::recall::RecallStore;
    use rustykrab_core::todo::TodoStore;
    use rustykrab_core::work::{PlanRejected, PlanWarning, WarningCheck};
    use rustykrab_core::ToolErrorKind;

    fn tool(stub: &Arc<StubWorkBackend>) -> WorkFileTool {
        WorkFileTool::new(stub.clone())
    }

    fn good() -> Value {
        json!({
            "title": "Book the dentist",
            "objective": "Get a cleaning appointment in October",
            "done_when": "An appointment is on the calendar",
        })
    }

    fn with(mut base: Value, key: &str, v: Value) -> Value {
        base[key] = v;
        base
    }

    fn files(stub: &StubWorkBackend) -> Vec<(WorkItemDraft, crate::work_backend::Provenance)> {
        stub.calls()
            .into_iter()
            .filter_map(|c| match c {
                WorkCall::File { draft, provenance } => Some((draft, provenance)),
                _ => None,
            })
            .collect()
    }

    struct Fixture(&'static str);
    #[async_trait]
    impl Tool for Fixture {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "fixture"
        }
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: self.0.into(),
                description: "fixture".into(),
                parameters: json!({ "type": "object" }),
            }
        }
        async fn execute(&self, _: Value) -> Result<Value> {
            Ok(Value::Null)
        }
    }

    fn session(allowed: &[&str], active: &[&str]) -> SessionToolContext {
        let conversation_id = uuid::Uuid::new_v4();
        let registry = ActiveToolsRegistry::new();
        registry.activate(conversation_id, active.iter().copied());
        SessionToolContext {
            conversation_id,
            capabilities: Arc::new(CapabilitySet::for_tools(allowed)),
            all_tools: Arc::new(vec![
                Arc::new(Fixture("browser")) as Arc<dyn Tool>,
                Arc::new(Fixture("gmail")),
            ]),
            active_tools: Arc::new(registry),
            recall: Arc::new(RecallStore::new()),
            todos: Arc::new(TodoStore::new()),
        }
    }

    #[test]
    fn schema_is_strict_and_needs_the_three_text_fields() {
        let stub = Arc::new(StubWorkBackend::new());
        let schema = tool(&stub).schema();
        assert_eq!(schema.name, "work_file");
        let p = &schema.parameters;
        assert_eq!(p["additionalProperties"], json!(false));
        assert_eq!(p["required"], json!(["title", "objective", "done_when"]));
        let props = p["properties"].as_object().unwrap();
        for key in DRAFT_KEYS {
            assert!(props.contains_key(key), "missing {key}");
        }
        for host in [
            "status",
            "budget",
            "tmp",
            "conversation_id",
            "preconditions",
        ] {
            assert!(
                !props.contains_key(host),
                "{host} must not be model-settable"
            );
        }
        assert!(!p["properties"]["kind"]["enum"]
            .as_array()
            .unwrap()
            .contains(&json!("code")));
    }

    #[tokio::test]
    async fn a_project_binding_reaches_the_work_backend() {
        let stub = Arc::new(StubWorkBackend::new());
        let out = tool(&stub).execute(json!({
            "title": "Continue project", "objective": "Build the next part", "done_when": "Verified",
            "artifact_refs": [{ "kind": "project", "value": "d88d4600-0000-4000-8000-000000000001" }]
        })).await.unwrap();
        assert_eq!(out["outcome"], "accepted", "{out}");
        assert_eq!(files(&stub)[0].0.artifact_refs[0].kind, "project");
    }

    #[tokio::test]
    async fn a_good_draft_is_filed_with_every_field_mapped() {
        let stub = Arc::new(StubWorkBackend::new().with_tool("browser", ToolState::Loaded));
        let args = json!({
            "title": "Compare phone plans",
            "objective": "Find the cheapest of three plans",
            "done_when": "A table of three plans with prices",
            "kind": "research",
            "constraints": ["UK carriers only", "UK carriers only"],
            "decisions_made": ["skip prepaid"],
            "artifact_refs": [{ "kind": "url", "value": "https://example.com/plans" }],
            "required_tools": ["browser"],
            "worker_kind": "local",
            "writable_resources": [],
            "parent": "#p-1",
            "inputs_from": ["a-1"],
            "edges": [{ "kind": "blocks", "depends_on": "a-1" }],
            "supersedes": "old-1",
            "trigger": { "kind": "at", "value": "2030-01-01T09:00:00Z" },
            "expires_at": "2030-02-01T00:00:00Z",
            "priority": 5,
            "plan": true
        });
        let out = tool(&stub).execute(args).await.unwrap();
        assert_eq!(out["outcome"], "accepted", "{out}");
        assert_eq!(out["id"], "stub-1");
        assert!(out["note"].as_str().unwrap().contains("planning step"));

        let filed = files(&stub);
        assert_eq!(filed.len(), 1);
        let d = &filed[0].0;
        assert_eq!(d.kind, Some(WorkKind::Research));
        assert_eq!(d.constraints, vec!["UK carriers only"], "deduplicated");
        assert_eq!(
            d.parent,
            Some(ItemRef::Id("p-1".into())),
            "leading # stripped"
        );
        assert_eq!(d.inputs_from, vec![ItemRef::Id("a-1".into())]);
        assert_eq!(
            d.edges,
            vec![DraftEdge {
                kind: EdgeKind::Blocks,
                depends_on: ItemRef::Id("a-1".into())
            }]
        );
        assert_eq!(d.supersedes.as_deref(), Some("old-1"));
        assert!(matches!(d.trigger, Trigger::At(_)));
        assert!(d.expires_at.is_some());
        assert_eq!(d.priority, 5);
        assert!(d.plan);
        assert_eq!(d.worker_kind, WorkerKind::Local);
        assert!(d.budget.is_none() && d.tmp.is_none() && d.preconditions.is_empty());
    }

    #[tokio::test]
    async fn malformed_args_fail_schema_validation_before_any_check() {
        let stub = Arc::new(StubWorkBackend::new());
        let t = tool(&stub);
        let err = t
            .execute(json!({ "objective": "o", "done_when": "d" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("'title'"), "{err}");
        assert_eq!(err.kind(), ToolErrorKind::InvalidInput);

        let err = t
            .execute(with(good(), "priority", json!("high")))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("priority"), "{err}");

        let err = t
            .execute(with(good(), "kind", json!("code")))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("'research'"), "{err}");
        assert!(stub.calls().is_empty());
    }

    #[tokio::test]
    async fn every_failed_draft_check_comes_back_at_once() {
        let stub = Arc::new(StubWorkBackend::new());
        let args = json!({
            "title": "  ",
            "objective": "x".repeat(OBJECTIVE_MAX + 1),
            "done_when": "d",
            "edges": [
                { "kind": "discovered_from", "depends_on": "a" },
                { "kind": "supersedes", "depends_on": "b" }
            ],
            "supersedes": "c",
            "expires_at": "2001-01-01T00:00:00Z",
            "status": "done",
            "budget": { "iterations": 99 }
        });
        let out = tool(&stub).execute(args).await.unwrap();
        assert_eq!(out["outcome"], "rejected");
        let failed = out["failed"].to_string();
        for needle in [
            "title: is empty",
            "objective: is 801 chars",
            "discovered_from",
            "at most one supersedes target",
            "expires_at: is in the past",
            "status: not yours to set: the controller sets status",
            "budget: not yours to set",
        ] {
            assert!(failed.contains(needle), "missing {needle:?} in {failed}");
        }
        assert!(
            stub.calls().is_empty(),
            "a rejected draft never reaches the backend"
        );
    }

    #[tokio::test]
    async fn a_registered_but_unloaded_tool_is_rejected_with_load_it() {
        let stub =
            Arc::new(StubWorkBackend::new().with_tool("browser", ToolState::RegisteredUnloaded));
        let out = tool(&stub)
            .execute(with(good(), "required_tools", json!(["browser"])))
            .await
            .unwrap();
        assert_eq!(out["outcome"], "rejected");
        let line = out["failed"][0].as_str().unwrap();
        assert!(line.contains("load it"), "{line}");
        assert!(line.contains("`browser`"), "{line}");
        assert!(stub.calls().is_empty());
    }

    #[tokio::test]
    async fn in_a_session_loaded_means_active_for_this_conversation() {
        // The backend says nothing; the session settles it.
        let stub = Arc::new(StubWorkBackend::new());
        let args = with(good(), "required_tools", json!(["browser"]));

        let unloaded = SESSION_TOOL_CONTEXT
            .scope(
                session(&["browser"], &[]),
                tool(&stub).execute(args.clone()),
            )
            .await
            .unwrap();
        assert_eq!(unloaded["outcome"], "rejected");
        assert!(unloaded["failed"][0].as_str().unwrap().contains("load it"));

        let loaded = SESSION_TOOL_CONTEXT
            .scope(
                session(&["browser"], &["browser"]),
                tool(&stub).execute(args),
            )
            .await
            .unwrap();
        assert_eq!(loaded["outcome"], "accepted", "{loaded}");
    }

    #[tokio::test]
    async fn a_tool_a_search_appended_counts_as_loaded() {
        let stub = Arc::new(StubWorkBackend::new());
        let ctx = session(&["browser"], &[]);
        ctx.active_tools.append(ctx.conversation_id, ["browser"]);
        let out = SESSION_TOOL_CONTEXT
            .scope(
                ctx,
                tool(&stub).execute(with(good(), "required_tools", json!(["browser"]))),
            )
            .await
            .unwrap();
        assert_eq!(out["outcome"], "accepted", "{out}");
    }

    #[tokio::test]
    async fn a_tool_this_caller_may_not_load_is_filed_when_the_host_has_it() {
        // `gmail` is registered but outside this caller's ceiling: "load it"
        // would be advice it cannot follow, so the host registry decides.
        let stub =
            Arc::new(StubWorkBackend::new().with_tool("gmail", ToolState::RegisteredUnloaded));
        let out = SESSION_TOOL_CONTEXT
            .scope(
                session(&["browser"], &[]),
                tool(&stub).execute(with(good(), "required_tools", json!(["gmail"]))),
            )
            .await
            .unwrap();
        assert_eq!(out["outcome"], "accepted", "{out}");
    }

    #[tokio::test]
    async fn an_unknown_tool_is_rejected_by_name() {
        let stub = Arc::new(StubWorkBackend::new());
        let out = tool(&stub)
            .execute(with(good(), "required_tools", json!(["weather_now"])))
            .await
            .unwrap();
        assert_eq!(out["outcome"], "rejected");
        let line = out["failed"][0].as_str().unwrap();
        assert!(
            line.contains("`weather_now` is not a registered tool"),
            "{line}"
        );
    }

    #[tokio::test]
    async fn an_unconfigured_mcp_server_is_accepted_as_needs_tool() {
        let stub = Arc::new(StubWorkBackend::new().with_mcp_server("linear"));
        let out = tool(&stub)
            .execute(with(
                good(),
                "required_mcp_servers",
                json!(["jira", "linear"]),
            ))
            .await
            .unwrap();
        assert_eq!(out["outcome"], "accepted", "{out}");
        assert_eq!(out["waits"], "needs_tool");
        assert_eq!(out["needs"], json!(["mcp:jira"]));
        let note = out["note"].as_str().unwrap();
        assert!(
            note.contains("needs_tool") && note.contains("`jira`"),
            "{note}"
        );
        assert_eq!(files(&stub).len(), 1);
    }

    #[tokio::test]
    async fn provenance_comes_from_the_host_never_from_args() {
        let stub = Arc::new(StubWorkBackend::new());
        let t = tool(&stub);

        // A model that tries to set provenance is refused outright.
        let mut forged = good();
        forged["conversation_id"] = json!("forged-conv");
        forged["actor"] = json!("controller");
        forged["filed_by_item"] = json!("forged-item");
        let out = t.execute(forged).await.unwrap();
        assert_eq!(out["outcome"], "rejected");
        assert!(out["failed"]
            .to_string()
            .contains("the host fills provenance"));
        assert!(stub.calls().is_empty());

        // Outside any runner scope: no conversation, default actor.
        t.execute(good()).await.unwrap();
        let (_, prov) = files(&stub).pop().unwrap();
        assert_eq!(prov.conversation_id, None);
        assert_eq!(prov.filed_by_item, None);
        assert_eq!(prov.actor, DEFAULT_ACTOR);

        // Inside a session and a worker run: both fill it.
        let ctx = session(&[], &[]);
        let conv = ctx.conversation_id.to_string();
        let run = WorkRunContext {
            item: "item-9".into(),
            actor: "worker:pinch".into(),
        };
        SESSION_TOOL_CONTEXT
            .scope(ctx, WORK_RUN_CONTEXT.scope(run, t.execute(good())))
            .await
            .unwrap();
        let (_, prov) = files(&stub).pop().unwrap();
        assert_eq!(prov.conversation_id.as_deref(), Some(conv.as_str()));
        assert_eq!(prov.filed_by_item.as_deref(), Some("item-9"));
        assert_eq!(prov.actor, "worker:pinch");
    }

    #[tokio::test]
    async fn a_backend_rejection_lists_every_failed_check() {
        let stub = Arc::new(StubWorkBackend::new());
        stub.push_outcome(PlanOutcome::Rejected(PlanRejected {
            failed: vec![
                FailedCheck {
                    reason: RejectionReason::SupersedesActive,
                    offending: vec![ItemRef::Id("old-1".into())],
                    detail: "old-1 is running".into(),
                },
                FailedCheck {
                    reason: RejectionReason::Cycle,
                    offending: vec![],
                    detail: String::new(),
                },
            ],
        }));
        let out = tool(&stub).execute(good()).await.unwrap();
        assert_eq!(
            out["failed"],
            json!(["supersedes_active (old-1): old-1 is running", "cycle"])
        );
    }

    #[test]
    fn accepted_outcomes_render_held_and_warnings() {
        let outcome = PlanOutcome::Accepted(rustykrab_core::work::PlanAccepted {
            root: "r-1".into(),
            ids: Default::default(),
            held: vec!["r-1".into()],
            policy: None,
            warnings: vec![PlanWarning {
                check: WarningCheck::SequentialSplit,
                items: vec!["a".into(), "b".into()],
            }],
        });
        let out = render_outcome(&outcome, &WorkItemDraft::default(), &[]);
        assert_eq!(out["held"], json!(true));
        assert_eq!(out["warnings"], json!(["sequential_split: a b"]));
        assert!(out.get("waits").is_none());
    }

    #[test]
    fn work_tools_registers_the_three_tools() {
        let tools = crate::work_tools(Arc::new(StubWorkBackend::new()));
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert_eq!(names, ["work_file", "work_status", "result_report"]);
    }
}
