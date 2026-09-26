//! `work_status`: read the work items the caller may see (plan sections 14
//! and 14.2).
//!
//! One compact line per item, capped, so a status read never floods a small
//! model's context: id, kind, status with its reason and cascade origin,
//! title, parent, edges in words, and roll-up counts for a parent.

use std::sync::Arc;

use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::work::{EdgeKind, Trigger};
use rustykrab_core::{validate_tool_args, Error, Result, Tool, ToolError};
use serde_json::{json, Value};

use crate::work_backend::{
    host_principal, StatusQuery, StatusSelector, WorkBackend, WorkStatusView,
};
use crate::work_file::{check_keys, Problem, POINTER_MAX};

/// Lines per call. A policy-capped graph (12 items) and its parent fit.
pub(crate) const STATUS_CAP: usize = 15;
/// Ids per call.
const IDS_MAX: usize = 20;
/// Title characters per line.
const TITLE_CLIP: usize = 60;
/// Ids named per edge phrase.
const EDGE_IDS_MAX: usize = 6;

/// Reads work items through a [`WorkBackend`].
pub struct WorkStatusTool {
    backend: Arc<dyn WorkBackend>,
}

impl WorkStatusTool {
    pub fn new(backend: Arc<dyn WorkBackend>) -> Self {
        Self { backend }
    }
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::ToolExecution(ToolError::invalid_input(msg))
}

fn clip(s: &str, max: usize) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        return one_line;
    }
    let mut out: String = one_line.chars().take(max.saturating_sub(3)).collect();
    out.push_str("...");
    out
}

fn ids_phrase(label: &str, ids: &[&str]) -> String {
    let shown = ids.iter().take(EDGE_IDS_MAX).copied().collect::<Vec<_>>();
    let mut out = format!("{label} {}", shown.join(" "));
    if ids.len() > EDGE_IDS_MAX {
        out.push_str(&format!(" +{}", ids.len() - EDGE_IDS_MAX));
    }
    out
}

/// The item's edges in words, upstreams first, grouped by relation.
fn edge_phrases(view: &WorkStatusView) -> Vec<String> {
    let id = view.item.id.as_str();
    let upstream = |kind: EdgeKind| -> Vec<&str> {
        view.edges
            .iter()
            .filter(|e| e.item == id && e.kind == kind)
            .map(|e| e.depends_on.as_str())
            .collect()
    };
    let downstream = |kind: EdgeKind| -> Vec<&str> {
        view.edges
            .iter()
            .filter(|e| e.depends_on == id && e.kind == kind)
            .map(|e| e.item.as_str())
            .collect()
    };
    let phrases = [
        ("blocked by", upstream(EdgeKind::Blocks)),
        ("waits for", upstream(EdgeKind::WaitsFor)),
        ("plan B of", upstream(EdgeKind::ConditionalOnFailure)),
        ("replaces", upstream(EdgeKind::Supersedes)),
        ("found during", upstream(EdgeKind::DiscoveredFrom)),
        ("blocks", downstream(EdgeKind::Blocks)),
        ("awaited by", downstream(EdgeKind::WaitsFor)),
        ("plan B is", downstream(EdgeKind::ConditionalOnFailure)),
        ("replaced by", downstream(EdgeKind::Supersedes)),
        ("led to", downstream(EdgeKind::DiscoveredFrom)),
    ];
    phrases
        .into_iter()
        .filter(|(_, ids)| !ids.is_empty())
        .map(|(label, ids)| ids_phrase(label, &ids))
        .collect()
}

/// One line per item, in the shape of plan section 14.2's `work show`.
pub(crate) fn render_line(view: &WorkStatusView) -> String {
    let item = &view.item;
    let status = view.rollup.unwrap_or(item.status);
    let mut line = format!("{} {} {}", item.id, item.kind.as_str(), status);
    if let Some(origin) = &item.status_origin {
        line.push_str(&format!(" <- {origin}"));
    }
    line.push_str(&format!(" \"{}\"", clip(&item.title, TITLE_CLIP)));
    if let Some(parent) = &view.parent {
        line.push_str(&format!("; parent {parent}"));
    }
    if view.children_total > 0 {
        line.push_str(&format!(
            "; {}/{} done",
            view.children_done, view.children_total
        ));
    }
    for phrase in edge_phrases(view) {
        line.push_str("; ");
        line.push_str(&phrase);
    }
    if !item.inputs_from.is_empty() {
        let ids: Vec<&str> = item.inputs_from.iter().map(String::as_str).collect();
        line.push_str("; ");
        line.push_str(&ids_phrase("inputs from", &ids));
    }
    match &item.trigger {
        Trigger::Now => {}
        Trigger::At(t) => line.push_str(&format!("; starts {}", t.format("%Y-%m-%dT%H:%MZ"))),
        Trigger::OnCredential(n) => line.push_str(&format!("; on credential {}", clip(n, 40))),
        Trigger::OnMcp(n) => line.push_str(&format!("; on MCP {}", clip(n, 40))),
        Trigger::OnAnswer(n) => line.push_str(&format!("; on answer {}", clip(n, 40))),
    }
    line
}

fn parse_id(v: &Value, field: &str) -> Result<String> {
    let id = v
        .as_str()
        .map(|s| s.trim().trim_start_matches('#').trim())
        .filter(|s| !s.is_empty() && s.chars().count() <= POINTER_MAX)
        .ok_or_else(|| invalid(format!("{field} must be an item id")))?;
    Ok(id.to_string())
}

fn parse_query(args: &Value) -> Result<StatusQuery> {
    let empty = serde_json::Map::new();
    let obj = args.as_object().unwrap_or(&empty);
    let mut problems: Vec<Problem> = Vec::new();
    check_keys(obj, &["ids", "root", "include_closed"], "", &mut problems);
    if !problems.is_empty() {
        let lines: Vec<String> = problems.iter().map(|p| p.detail.clone()).collect();
        return Err(invalid(lines.join("; ")));
    }
    let include_closed = obj
        .get("include_closed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let ids = obj.get("ids").filter(|v| !v.is_null());
    let root = obj.get("root").filter(|v| !v.is_null());
    let select = match (ids, root) {
        (Some(_), Some(_)) => return Err(invalid("pass ids or root, not both")),
        (None, None) => {
            return Err(invalid(
                "pass ids (items to read) or root (an item and its subtree)",
            ))
        }
        (None, Some(root)) => StatusSelector::Root(parse_id(root, "root")?),
        (Some(ids), None) => {
            let arr = ids
                .as_array()
                .ok_or_else(|| invalid("ids must be an array of item ids"))?;
            if arr.is_empty() {
                return Err(invalid("ids is empty; name at least one item"));
            }
            if arr.len() > IDS_MAX {
                return Err(invalid(format!(
                    "ids has {} entries; read at most {IDS_MAX} at a time",
                    arr.len()
                )));
            }
            let mut out: Vec<String> = Vec::new();
            for (i, v) in arr.iter().enumerate() {
                let id = parse_id(v, &format!("ids[{i}]"))?;
                if !out.contains(&id) {
                    out.push(id);
                }
            }
            StatusSelector::Ids(out)
        }
    };
    Ok(StatusQuery {
        select,
        include_closed,
    })
}

#[async_trait]
impl Tool for WorkStatusTool {
    fn name(&self) -> &str {
        "work_status"
    }

    fn description(&self) -> &str {
        "Read work items you may see. Pass ids, or root for an item and everything under \
         it. Each line: id, kind, status (with the item that caused a cascade after <-), \
         title, parent, edges in words, and done counts for a parent. Closed items under a \
         root are hidden unless include_closed is true."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "ids": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": format!("Item ids to read (max {IDS_MAX}).")
                    },
                    "root": {
                        "type": "string",
                        "description": "An item id: read it and its subtree."
                    },
                    "include_closed": {
                        "type": "boolean",
                        "description": "Include done, failed, cancelled and expired items under root. Default false."
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let schema = self.schema();
        validate_tool_args(&schema.parameters, &args).map_err(Error::ToolExecution)?;
        let query = parse_query(&args)?;
        let asked: Vec<String> = match &query.select {
            StatusSelector::Ids(ids) => ids.clone(),
            StatusSelector::Root(_) => Vec::new(),
        };

        let views = self
            .backend
            .status(query, &host_principal())
            .await
            .map_err(|e| {
                Error::ToolExecution(ToolError::internal(format!("work_status failed: {e}")))
            })?;

        let lines: Vec<String> = views.iter().take(STATUS_CAP).map(render_line).collect();
        let mut out = json!({ "items": lines });
        if views.len() > STATUS_CAP {
            out["more"] = json!(views.len() - STATUS_CAP);
        }
        let missing: Vec<&String> = asked
            .iter()
            .filter(|id| !views.iter().any(|v| &v.item.id == *id))
            .collect();
        if !missing.is_empty() {
            out["missing"] = json!(missing);
            out["note"] = json!("missing ids do not exist or are not yours to see");
        } else if views.is_empty() {
            out["note"] = json!("no visible items");
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_backend::{
        fixture_view, Principal, StubWorkBackend, WorkCall, WorkRunContext, WORK_RUN_CONTEXT,
    };
    use rustykrab_core::work::{BlockedReason, CancelReason, Edge, Status, WorkKind};
    use rustykrab_core::ToolErrorKind;

    fn child(id: &str, parent: &str, status: Status) -> WorkStatusView {
        let mut v = fixture_view(id, &format!("step {id}"), status);
        v.parent = Some(parent.to_string());
        v.item.parent = Some(parent.to_string());
        v
    }

    fn edge(item: &str, kind: EdgeKind, depends_on: &str) -> Edge {
        Edge {
            item: item.into(),
            depends_on: depends_on.into(),
            kind,
        }
    }

    #[test]
    fn schema_takes_ids_or_root_and_nothing_else() {
        let schema = WorkStatusTool::new(Arc::new(StubWorkBackend::new())).schema();
        assert_eq!(schema.name, "work_status");
        let props = schema.parameters["properties"].as_object().unwrap();
        let mut keys: Vec<&str> = props.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(keys, ["ids", "include_closed", "root"]);
        assert_eq!(schema.parameters["additionalProperties"], json!(false));
    }

    #[tokio::test]
    async fn bad_args_are_refused_before_the_backend() {
        let stub = Arc::new(StubWorkBackend::new());
        let t = WorkStatusTool::new(stub.clone());
        for (args, needle) in [
            (json!({}), "pass ids"),
            (json!({ "ids": ["a"], "root": "b" }), "not both"),
            (json!({ "ids": "a" }), "'ids' must be array"),
            (json!({ "ids": [] }), "ids is empty"),
            (json!({ "ids": [7] }), "ids[0] must be an item id"),
            (json!({ "root": "r", "status": "done" }), "not yours to set"),
            (json!({ "ids": vec!["x"; IDS_MAX + 1] }), "at most"),
        ] {
            let err = t.execute(args.clone()).await.unwrap_err();
            assert_eq!(err.kind(), ToolErrorKind::InvalidInput, "{args}");
            assert!(err.to_string().contains(needle), "{args}: {err}");
        }
        assert!(stub.calls().is_empty());
    }

    #[tokio::test]
    async fn a_parent_line_carries_rollup_counts_origin_and_edges_in_words() {
        let mut parent = fixture_view("p", "Plan the Lisbon trip", Status::Running);
        parent.item.kind = WorkKind::Personal;
        parent.rollup = Some(Status::Blocked(BlockedReason::UpstreamFailed));
        parent.item.status_origin = Some("c".into());
        parent.children_done = 1;
        parent.children_total = 4;

        let mut held = child("d", "p", Status::Blocked(BlockedReason::UpstreamFailed));
        held.item.status_origin = Some("c".into());
        held.item.inputs_from = vec!["b".into(), "c".into()];
        held.edges = vec![
            edge("d", EdgeKind::Blocks, "b"),
            edge("d", EdgeKind::Blocks, "c"),
            edge("e", EdgeKind::Blocks, "d"),
            edge("f", EdgeKind::ConditionalOnFailure, "d"),
        ];

        let stub = Arc::new(StubWorkBackend::new().with_item(parent).with_item(held));
        let out = WorkStatusTool::new(stub)
            .execute(json!({ "ids": ["p", "d"] }))
            .await
            .unwrap();
        assert_eq!(
            out["items"],
            json!([
                "p personal blocked(upstream_failed) <- c \"Plan the Lisbon trip\"; 1/4 done",
                "d personal blocked(upstream_failed) <- c \"step d\"; parent p; blocked by b c; \
                 blocks e; plan B is f; inputs from b c"
            ])
        );
        assert!(out.get("more").is_none() && out.get("missing").is_none());
    }

    #[tokio::test]
    async fn output_stays_within_the_cap_and_counts_the_rest() {
        let mut stub =
            StubWorkBackend::new().with_item(fixture_view("root", "big", Status::Running));
        let n = 40;
        for i in 0..n {
            let mut v = child(&format!("c{i}"), "root", Status::Ready);
            v.item.title = "a very long title that keeps going ".repeat(10);
            stub = stub.with_item(v);
        }
        let out = WorkStatusTool::new(Arc::new(stub))
            .execute(json!({ "root": "root" }))
            .await
            .unwrap();
        let items = out["items"].as_array().unwrap();
        assert_eq!(items.len(), STATUS_CAP);
        assert_eq!(out["more"], json!(n + 1 - STATUS_CAP));
        for line in items {
            assert!(line.as_str().unwrap().len() < 200, "{line}");
        }
    }

    #[tokio::test]
    async fn missing_ids_are_named_and_closed_children_hidden_by_default() {
        let stub = Arc::new(
            StubWorkBackend::new()
                .with_item(fixture_view("r", "root", Status::Running))
                .with_item(child("a", "r", Status::Done))
                .with_item(child("b", "r", Status::Cancelled(CancelReason::Superseded))),
        );
        let t = WorkStatusTool::new(stub);
        let out = t.execute(json!({ "ids": ["r", "ghost"] })).await.unwrap();
        assert_eq!(out["missing"], json!(["ghost"]));

        let open = t.execute(json!({ "root": "r" })).await.unwrap();
        assert_eq!(open["items"].as_array().unwrap().len(), 1);
        let all = t
            .execute(json!({ "root": "r", "include_closed": true }))
            .await
            .unwrap();
        assert_eq!(all["items"].as_array().unwrap().len(), 3);
        assert!(all["items"][2]
            .as_str()
            .unwrap()
            .contains("cancelled(superseded)"));
    }

    #[tokio::test]
    async fn the_principal_is_the_host_binding() {
        let stub = Arc::new(StubWorkBackend::new());
        let t = WorkStatusTool::new(stub.clone());
        let run = WorkRunContext {
            item: "item-3".into(),
            actor: "worker:krabby".into(),
        };
        let out = WORK_RUN_CONTEXT
            .scope(run, t.execute(json!({ "root": "#item-3" })))
            .await
            .unwrap();
        assert_eq!(out["note"], "no visible items");
        assert_eq!(
            stub.calls(),
            vec![WorkCall::Status {
                query: StatusQuery {
                    select: StatusSelector::Root("item-3".into()),
                    include_closed: false,
                },
                principal: Principal {
                    conversation_id: None,
                    item: Some("item-3".into()),
                },
            }]
        );
    }
}
