//! Graph engine tests, one function per plan scenario or rule.
//!
//! - `lisbon`: the worked example of plan section 4.1, end to end.
//! - `scenarios`: plan section 15, scenarios 18 to 26 and 28.
//! - `rollup_table`: every row of 4.2's roll-up table.
//! - `rejections`: every rejection reason of 14.1, and all at once.
//! - `rules`: readiness, cascade, holds, plan B chains and approval.

use chrono::{DateTime, TimeDelta, TimeZone, Utc};
use rustykrab_core::work::{
    Budget, Edge, EdgeKind, ItemRef, PlanEdge, Status, Trigger, WorkItem, WorkItemDraft,
    WorkItemId, WorkKind, WorkPlan,
};

use super::{
    settle, step, validate, Accepted, Effects, FilingContext, FilingSource, Rejection, Snapshot,
    Validation,
};

mod lisbon;
mod rejections;
mod rollup_table;
mod rules;
mod scenarios;

/// Hours after a fixed "now", so tests read as a timeline.
fn t(hours: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 4, 20, 9, 0, 0).unwrap() + TimeDelta::hours(hours)
}

fn now() -> DateTime<Utc> {
    t(0)
}

fn small() -> Budget {
    Budget {
        iterations: 10,
        tokens: 10_000,
        wall_seconds: 600,
        ..Budget::default()
    }
}

fn big() -> Budget {
    Budget {
        iterations: 1_000,
        tokens: 1_000_000,
        wall_seconds: 60_000,
        ..Budget::default()
    }
}

/// A stored row: a queued personal item with a small budget.
fn row(id: &str) -> WorkItem {
    WorkItem {
        id: id.to_string(),
        kind: WorkKind::Personal,
        title: id.to_string(),
        objective: format!("do {id}"),
        done_when: format!("{id} is done"),
        constraints: vec![],
        decisions_made: vec![],
        artifact_refs: vec![],
        required_tools: vec![],
        required_mcp_servers: vec![],
        worker_kind: Default::default(),
        writable_resources: vec![],
        parent: None,
        inputs_from: vec![],
        origin_conversation_id: None,
        trigger: Trigger::Now,
        preconditions: vec![],
        expires_at: None,
        budget: small(),
        priority: 0,
        status: Status::Queued,
        status_origin: None,
        plan_id: None,
        held_by: None,
        created_at: t(-48),
        updated_at: t(-48),
        closed_at: None,
    }
}

/// A small builder for stored graphs.
#[derive(Default)]
struct G {
    items: Vec<WorkItem>,
    edges: Vec<Edge>,
}

impl G {
    fn new() -> G {
        G::default()
    }

    fn add(mut self, id: &str) -> G {
        self.items.push(row(id));
        self
    }

    fn child(mut self, id: &str, parent: &str) -> G {
        let mut r = row(id);
        r.parent = Some(parent.to_string());
        self.items.push(r);
        self
    }

    fn set(mut self, id: &str, f: impl FnOnce(&mut WorkItem)) -> G {
        let r = self
            .items
            .iter_mut()
            .find(|i| i.id == id)
            .unwrap_or_else(|| panic!("no item {id}"));
        f(r);
        self
    }

    fn status(self, id: &str, s: Status) -> G {
        self.set(id, |r| {
            r.status = s;
            if s.is_closed() {
                r.closed_at = Some(t(-1));
            }
        })
    }

    /// `down` holds an edge of `kind` naming `up`.
    fn edge(mut self, down: &str, kind: EdgeKind, up: &str) -> G {
        self.edges.push(Edge {
            item: down.to_string(),
            depends_on: up.to_string(),
            kind,
        });
        self
    }

    fn snap(&self) -> Snapshot {
        Snapshot::new(self.items.clone(), self.edges.clone())
    }
}

fn st(s: &Snapshot, id: &str) -> Status {
    s.status(id).unwrap_or_else(|| panic!("no item {id}"))
}

fn origin(s: &Snapshot, id: &str) -> Option<String> {
    s.item(id).and_then(|i| i.status_origin.clone())
}

/// One controller step, applied to the snapshot.
fn go(s: &mut Snapshot, id: &str, to: Status) -> Effects {
    let fx = step(s, id, to, now());
    s.apply(&fx, now());
    fx
}

fn draft(tmp: &str) -> WorkItemDraft {
    WorkItemDraft {
        tmp: Some(tmp.to_string()),
        title: tmp.to_string(),
        objective: format!("do {tmp}"),
        done_when: format!("{tmp} is done"),
        ..Default::default()
    }
}

fn tmp(t: &str) -> ItemRef {
    ItemRef::Tmp { tmp: t.to_string() }
}

fn id(i: &str) -> ItemRef {
    ItemRef::Id(i.to_string())
}

fn pe(item: ItemRef, kind: EdgeKind, depends_on: ItemRef) -> PlanEdge {
    PlanEdge {
        item,
        kind,
        depends_on,
    }
}

fn plan(root: ItemRef, items: Vec<WorkItemDraft>, edges: Vec<PlanEdge>) -> WorkPlan {
    WorkPlan {
        root,
        items,
        edges,
        rationale: String::new(),
    }
}

fn ctx(source: FilingSource) -> FilingContext {
    let mut c = FilingContext::new(source, now());
    c.default_budget = small();
    c
}

/// Validate, insert and settle, as the controller would; panics on a
/// rejection.
fn accept(s: &mut Snapshot, p: &WorkPlan, c: &FilingContext) -> Accepted {
    let a = match validate(s, p, c) {
        Validation::Accepted(a) => *a,
        Validation::Rejected(r) => panic!("rejected: {r:#?}"),
    };
    s.insert(&a, now());
    let fx = settle(s, &a.changed(), now());
    s.apply(&fx, now());
    a
}

fn reject(s: &Snapshot, p: &WorkPlan, c: &FilingContext) -> Rejection {
    match validate(s, p, c) {
        Validation::Rejected(r) => r,
        Validation::Accepted(a) => panic!("accepted: {a:#?}"),
    }
}

fn ids(a: &Accepted, tmps: &[&str]) -> Vec<WorkItemId> {
    tmps.iter()
        .map(|k| a.id(k).unwrap_or_else(|| panic!("no tmp {k}")).clone())
        .collect()
}

fn names(r: &Rejection) -> Vec<&'static str> {
    r.checks()
        .iter()
        .map(|c| match c {
            super::Check::Reason(reason) => reason.as_str(),
            super::Check::SequentialSplit => "sequential_split",
        })
        .collect()
}
