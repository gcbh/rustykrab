//! The delivery import (plan sections 4, 6.1 and 14.1): a code slice from
//! the delivery compiler's `StackManifest` becomes one layered graph.
//!
//! The graph is fixed by the plan, not chosen by a model: one parent item
//! for the slice; under it one child parent per stack layer, with the
//! layer's acceptance as its `done_when` and a `blocks` edge onto the layer
//! below; under each layer a `code` child per delivery work item, with its
//! `delivery_dependencies` as `blocks` edges. [`plan_of`] only builds the
//! [`WorkPlan`]; the controller files it through the same validator as
//! every other filing path, as `FilingSource::DeliveryImport`, the only
//! source that may file `code` items. A manifest that names an unknown
//! dependency or holds a cycle is therefore rejected whole with
//! `unknown_ref` or `cycle`, and nothing from it is stored.
//!
//! Until Phase 7 wires the live compiler, the manifest arrives as a
//! recorded fixture over `POST /api/work/import`; its shape here is the
//! subset of the delivery plan's `StackManifest` the import reads.

use rustykrab_core::work::{
    Budget, EdgeKind, ItemRef, PlanEdge, WorkItemDraft, WorkKind, WorkPlan, WorkerKind,
};
use serde::{Deserialize, Serialize};

/// The slice a manifest delivers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestSlice {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub objective: String,
}

/// One delivery work item: a `code` leaf of its layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestWorkItem {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub objective: String,
    #[serde(default)]
    pub done_when: String,
    /// Ids of work items (in any layer) that must land first.
    #[serde(default)]
    pub delivery_dependencies: Vec<String>,
}

/// One stack layer, ordered bottom to top.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestLayer {
    pub id: String,
    pub title: String,
    /// What becomes true at this layer: the layer parent's `done_when`.
    pub acceptance: String,
    /// The layer this one stacks on; the previous layer when absent.
    #[serde(default)]
    pub parent_layer: Option<String>,
    #[serde(default)]
    pub work_items: Vec<ManifestWorkItem>,
}

/// The delivery compiler's ordered stack for one slice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackManifest {
    pub slice: ManifestSlice,
    pub layers: Vec<ManifestLayer>,
}

/// The temp id of the slice's parent item.
pub const SLICE_TMP: &str = "slice";

/// The temp id of a layer's parent item.
pub fn layer_tmp(layer: &str) -> String {
    format!("layer:{layer}")
}

/// The temp id of a delivery work item.
pub fn work_item_tmp(item: &str) -> String {
    format!("item:{item}")
}

fn tmp(id: String) -> ItemRef {
    ItemRef::Tmp { tmp: id }
}

fn blocks(item: String, depends_on: String) -> PlanEdge {
    PlanEdge {
        item: tmp(item),
        kind: EdgeKind::Blocks,
        depends_on: tmp(depends_on),
    }
}

/// The sum of `budgets`, the envelope a parent needs to cover them.
fn envelope(budgets: &[Budget], base: Budget) -> Budget {
    let mut total = Budget {
        iterations: 0,
        tokens: 0,
        wall_seconds: 0,
        ..base
    };
    for b in budgets {
        total.iterations = total.iterations.saturating_add(b.iterations);
        total.tokens = total.tokens.saturating_add(b.tokens);
        total.wall_seconds = total.wall_seconds.saturating_add(b.wall_seconds);
    }
    total
}

/// The graph section 14.1 prescribes for `manifest`, rooted at the slice.
///
/// Every `code` leaf gets `leaf_budget`; each layer's budget is the sum of
/// its leaves' and the slice's the sum of its layers', so the envelope
/// check (4.2) never rejects an import for arithmetic the compiler did not
/// choose.
pub fn plan_of(manifest: &StackManifest, leaf_budget: Budget) -> WorkPlan {
    let slice = &manifest.slice;
    let mut items: Vec<WorkItemDraft> = Vec::new();
    let mut edges: Vec<PlanEdge> = Vec::new();
    let mut layer_budgets = Vec::new();

    let mut previous: Option<&str> = None;
    for layer in &manifest.layers {
        let layer_id = layer_tmp(&layer.id);
        let below = layer.parent_layer.as_deref().or(previous);
        if let Some(below) = below {
            edges.push(blocks(layer_id.clone(), layer_tmp(below)));
        }
        let leaves = vec![leaf_budget; layer.work_items.len()];
        let layer_budget = envelope(&leaves, leaf_budget);
        layer_budgets.push(layer_budget);
        items.push(WorkItemDraft {
            tmp: Some(layer_id.clone()),
            kind: Some(WorkKind::Code),
            title: layer.title.clone(),
            objective: format!("Stack layer {} of {}: {}", layer.id, slice.id, layer.title),
            done_when: layer.acceptance.clone(),
            worker_kind: WorkerKind::Any,
            parent: Some(tmp(SLICE_TMP.to_string())),
            budget: Some(layer_budget),
            ..WorkItemDraft::default()
        });
        for work in &layer.work_items {
            let work_id = work_item_tmp(&work.id);
            for dependency in &work.delivery_dependencies {
                edges.push(blocks(work_id.clone(), work_item_tmp(dependency)));
            }
            items.push(WorkItemDraft {
                tmp: Some(work_id),
                kind: Some(WorkKind::Code),
                title: work.title.clone(),
                objective: if work.objective.is_empty() {
                    work.title.clone()
                } else {
                    work.objective.clone()
                },
                done_when: if work.done_when.is_empty() {
                    layer.acceptance.clone()
                } else {
                    work.done_when.clone()
                },
                artifact_refs: vec![rustykrab_core::work::ArtifactRef {
                    kind: "delivery_work_item".to_string(),
                    value: work.id.clone(),
                }],
                parent: Some(tmp(layer_id.clone())),
                budget: Some(leaf_budget),
                ..WorkItemDraft::default()
            });
        }
        previous = Some(&layer.id);
    }

    items.insert(
        0,
        WorkItemDraft {
            tmp: Some(SLICE_TMP.to_string()),
            kind: Some(WorkKind::Code),
            title: slice.title.clone(),
            objective: if slice.objective.is_empty() {
                slice.title.clone()
            } else {
                slice.objective.clone()
            },
            done_when: "every stack layer's acceptance holds".to_string(),
            artifact_refs: vec![rustykrab_core::work::ArtifactRef {
                kind: "delivery_slice".to_string(),
                value: slice.id.clone(),
            }],
            budget: Some(envelope(&layer_budgets, leaf_budget)),
            ..WorkItemDraft::default()
        },
    );

    WorkPlan {
        root: tmp(SLICE_TMP.to_string()),
        items,
        edges,
        rationale: format!("delivery import of slice {}", slice.id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{validate, FilingContext, FilingSource, Snapshot, Validation};
    use chrono::Utc;
    use rustykrab_core::work::RejectionReason;

    fn work(id: &str, title: &str, deps: &[&str]) -> ManifestWorkItem {
        ManifestWorkItem {
            id: id.into(),
            title: title.into(),
            objective: format!("do {title}"),
            done_when: format!("{title} landed"),
            delivery_dependencies: deps.iter().map(|d| d.to_string()).collect(),
        }
    }

    /// The e2e suite's recorded fixture: two layers, three work items.
    fn manifest(cyclic: bool) -> StackManifest {
        let first: &[&str] = if cyclic { &["wi-2"] } else { &[] };
        StackManifest {
            slice: ManifestSlice {
                id: "slice-1".into(),
                title: "Work items over REST".into(),
                objective: "Persist work items and list them".into(),
            },
            layers: vec![
                ManifestLayer {
                    id: "layer-1".into(),
                    title: "Persist work items".into(),
                    acceptance: "work items survive a daemon restart".into(),
                    parent_layer: None,
                    work_items: vec![
                        work("wi-1", "Add the table", first),
                        work("wi-2", "Add the store API", &["wi-1"]),
                    ],
                },
                ManifestLayer {
                    id: "layer-2".into(),
                    title: "List work items over REST".into(),
                    acceptance: "GET /api/work lists open items".into(),
                    parent_layer: Some("layer-1".into()),
                    work_items: vec![work("wi-3", "Add the list route", &[])],
                },
            ],
        }
    }

    fn edge(plan: &WorkPlan, item: &str, depends_on: &str) -> bool {
        plan.edges.iter().any(|e| {
            e.kind == EdgeKind::Blocks
                && e.item == tmp(item.to_string())
                && e.depends_on == tmp(depends_on.to_string())
        })
    }

    #[test]
    fn a_manifest_becomes_one_parent_per_slice_and_layer_with_code_leaves() {
        let plan = plan_of(&manifest(false), Budget::default());
        assert_eq!(plan.root, tmp(SLICE_TMP.into()));
        assert_eq!(plan.items.len(), 6);
        let by_tmp = |t: &str| {
            plan.items
                .iter()
                .find(|i| i.tmp.as_deref() == Some(t))
                .unwrap()
        };
        assert_eq!(by_tmp(SLICE_TMP).title, "Work items over REST");
        assert!(by_tmp(SLICE_TMP).parent.is_none());
        let lower = by_tmp("layer:layer-1");
        assert_eq!(lower.done_when, "work items survive a daemon restart");
        assert_eq!(lower.parent, Some(tmp(SLICE_TMP.into())));
        for leaf in ["item:wi-1", "item:wi-2"] {
            assert_eq!(by_tmp(leaf).parent, Some(tmp("layer:layer-1".into())));
        }
        assert_eq!(
            by_tmp("item:wi-3").parent,
            Some(tmp("layer:layer-2".into()))
        );
        assert!(plan.items.iter().all(|i| i.kind == Some(WorkKind::Code)));
        assert!(edge(&plan, "layer:layer-2", "layer:layer-1"));
        assert!(edge(&plan, "item:wi-2", "item:wi-1"));
        assert_eq!(plan.edges.len(), 2);
    }

    #[test]
    fn a_missing_parent_layer_stacks_on_the_previous_one() {
        let mut m = manifest(false);
        m.layers[1].parent_layer = None;
        assert!(edge(
            &plan_of(&m, Budget::default()),
            "layer:layer-2",
            "layer:layer-1"
        ));
    }

    #[test]
    fn parents_budget_the_sum_of_what_they_hold() {
        let plan = plan_of(&manifest(false), Budget::default());
        let leaf = Budget::default();
        let slice = plan.items[0].budget.unwrap();
        assert_eq!(slice.tokens, leaf.tokens * 3);
        assert_eq!(slice.iterations, leaf.iterations * 3);
    }

    #[test]
    fn the_import_passes_the_validator_as_the_delivery_import_only() {
        let plan = plan_of(&manifest(false), Budget::default());
        let snap = Snapshot::new(Vec::new(), Vec::new());
        let ctx = FilingContext::new(FilingSource::DeliveryImport, Utc::now());
        match validate(&snap, &plan, &ctx) {
            Validation::Accepted(accepted) => assert_eq!(accepted.items.len(), 6),
            Validation::Rejected(r) => panic!("the import was rejected: {r:?}"),
        }
        let planner = FilingContext::new(FilingSource::Planner, Utc::now());
        match validate(&snap, &plan, &planner) {
            Validation::Rejected(r) => assert!(r.has(RejectionReason::KindNotAllowed)),
            Validation::Accepted(_) => panic!("a planner filed a code graph"),
        }
    }

    #[test]
    fn a_cyclic_manifest_is_rejected_whole_with_cycle() {
        let plan = plan_of(&manifest(true), Budget::default());
        let snap = Snapshot::new(Vec::new(), Vec::new());
        let ctx = FilingContext::new(FilingSource::DeliveryImport, Utc::now());
        match validate(&snap, &plan, &ctx) {
            Validation::Rejected(r) => assert!(r.has(RejectionReason::Cycle), "{r:?}"),
            Validation::Accepted(_) => panic!("a cyclic manifest was accepted"),
        }
    }

    #[test]
    fn the_fixture_json_parses() {
        let raw = serde_json::json!({
            "slice": { "id": "s", "title": "S", "objective": "o" },
            "layers": [ {
                "id": "l1", "title": "L1", "acceptance": "a", "parent_layer": null,
                "work_items": [ { "id": "w", "title": "W", "objective": "o",
                                  "done_when": "d", "delivery_dependencies": [] } ],
            } ],
        });
        let m: StackManifest = serde_json::from_value(raw).unwrap();
        assert_eq!(m.layers[0].work_items[0].id, "w");
    }
}
