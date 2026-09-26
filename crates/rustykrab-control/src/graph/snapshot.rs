//! The in-memory view every graph rule computes over.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use rustykrab_core::work::{Edge, Status, Trigger, WorkItem, WorkItemId};

use super::validate::Accepted;
use super::{Effects, Link};

/// Work items and their edges as the store holds them, indexed for the
/// rules of plan section 4: by id, the children of each parent, the edges
/// an item holds (its upstreams) and the edges naming an item (its
/// dependents).
///
/// The snapshot is whatever the store loaded: usually a root's subtree plus
/// every item its edges and `inputs_from` name. An upstream missing from
/// the snapshot is treated conservatively (never satisfied).
///
/// Iteration follows the order the rows were given in, so every result the
/// engine returns is deterministic.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    items: Vec<WorkItem>,
    index: HashMap<WorkItemId, usize>,
    edges: Vec<Edge>,
    children: HashMap<WorkItemId, Vec<WorkItemId>>,
    held: HashMap<WorkItemId, Vec<usize>>,
    naming: HashMap<WorkItemId, Vec<usize>>,
    fired: HashSet<WorkItemId>,
}

impl Snapshot {
    /// Index `items` and `edges`. A repeated item id keeps its first row; a
    /// repeated edge is kept once.
    pub fn new(items: Vec<WorkItem>, edges: Vec<Edge>) -> Snapshot {
        let mut snap = Snapshot::default();
        for item in items {
            if !snap.index.contains_key(&item.id) {
                snap.index.insert(item.id.clone(), snap.items.len());
                snap.items.push(item);
            }
        }
        let mut seen = HashSet::new();
        for edge in edges {
            if seen.insert(edge.clone()) {
                snap.edges.push(edge);
            }
        }
        snap.reindex_children();
        snap.reindex_edges();
        snap
    }

    /// Mark the items whose non-time trigger (`on_credential`, `on_mcp`,
    /// `on_answer`) has fired. `now` and `at(time)` triggers need no mark.
    pub fn with_fired<I: IntoIterator<Item = WorkItemId>>(mut self, ids: I) -> Snapshot {
        self.fired.extend(ids);
        self
    }

    /// Mark one item's non-time trigger as fired.
    pub fn fire(&mut self, id: &str) {
        self.fired.insert(id.to_string());
    }

    pub fn items(&self) -> &[WorkItem] {
        &self.items
    }

    pub fn edges(&self) -> &[Edge] {
        &self.edges
    }

    pub fn item(&self, id: &str) -> Option<&WorkItem> {
        self.index.get(id).map(|&i| &self.items[i])
    }

    pub fn contains(&self, id: &str) -> bool {
        self.index.contains_key(id)
    }

    pub fn status(&self, id: &str) -> Option<Status> {
        self.item(id).map(|i| i.status)
    }

    /// The children of `id`, in row order.
    pub fn children(&self, id: &str) -> &[WorkItemId] {
        self.children.get(id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Whether `id` is a parent. A parent is never ready and never leased
    /// (4.2).
    pub fn has_children(&self, id: &str) -> bool {
        !self.children(id).is_empty()
    }

    /// The edges `id` holds: `id` is the downstream, each names an upstream.
    pub fn edges_held_by(&self, id: &str) -> impl Iterator<Item = &Edge> + '_ {
        self.held
            .get(id)
            .into_iter()
            .flatten()
            .map(move |&i| &self.edges[i])
    }

    /// The edges naming `id` as their upstream: its dependents.
    pub fn edges_naming(&self, id: &str) -> impl Iterator<Item = &Edge> + '_ {
        self.naming
            .get(id)
            .into_iter()
            .flatten()
            .map(move |&i| &self.edges[i])
    }

    /// The ancestors of `id` present in the snapshot, nearest first. A
    /// corrupt parent loop stops at the first repeat.
    pub fn ancestors(&self, id: &str) -> Vec<WorkItemId> {
        let mut out: Vec<WorkItemId> = Vec::new();
        let mut current = self.item(id).and_then(|i| i.parent.clone());
        while let Some(parent) = current {
            if parent == id || out.contains(&parent) || !self.contains(&parent) {
                break;
            }
            current = self.item(&parent).and_then(|i| i.parent.clone());
            out.push(parent);
        }
        out
    }

    /// Every descendant of `id`, parents before their children.
    pub fn descendants(&self, id: &str) -> Vec<WorkItemId> {
        let mut out: Vec<WorkItemId> = Vec::new();
        let mut seen: HashSet<&str> = HashSet::from([id]);
        let mut stack: Vec<&WorkItemId> = self.children(id).iter().rev().collect();
        while let Some(child) = stack.pop() {
            if !seen.insert(child.as_str()) {
                continue;
            }
            out.push(child.clone());
            stack.extend(self.children(child).iter().rev());
        }
        out
    }

    /// Whether `id` is `root` or sits under it.
    pub fn is_within(&self, id: &str, root: &str) -> bool {
        (id == root && self.contains(id)) || self.ancestors(id).iter().any(|a| a == root)
    }

    /// Whether the item's own trigger has fired: `now` always, `at(t)` once
    /// `t <= now`, the others only when the snapshot marks them fired.
    pub fn trigger_fired(&self, id: &str, now: DateTime<Utc>) -> bool {
        match self.item(id).map(|i| &i.trigger) {
            Some(Trigger::Now) => true,
            Some(Trigger::At(t)) => *t <= now,
            Some(_) => self.fired.contains(id),
            None => false,
        }
    }

    /// Replay effects onto the snapshot: statuses (with `status_origin`,
    /// `updated_at` and `closed_at`), re-points and dropped edges.
    pub fn apply(&mut self, effects: &Effects, now: DateTime<Utc>) {
        for t in &effects.transitions {
            self.set_status(&t.item, t.to, t.origin.clone(), now);
        }
        for r in &effects.repoints {
            self.repoint(&r.item, r.link, &r.old_upstream, &r.new_upstream);
        }
        for e in &effects.dropped_edges {
            self.remove_edge(e);
        }
    }

    /// Insert an accepted filing: its new rows, then its effects on existing
    /// items.
    pub fn insert(&mut self, accepted: &Accepted, now: DateTime<Utc>) {
        for item in &accepted.items {
            self.add_item(item.clone());
        }
        for edge in &accepted.edges {
            self.add_edge(edge.clone());
        }
        self.apply(&accepted.effects, now);
    }

    /// Add one item row; a repeated id is ignored.
    pub fn add_item(&mut self, item: WorkItem) {
        if self.index.contains_key(&item.id) {
            return;
        }
        self.index.insert(item.id.clone(), self.items.len());
        if let Some(parent) = &item.parent {
            self.children
                .entry(parent.clone())
                .or_default()
                .push(item.id.clone());
        }
        self.items.push(item);
    }

    /// Add one edge row; a repeated edge is ignored.
    pub fn add_edge(&mut self, edge: Edge) {
        if self.edges.contains(&edge) {
            return;
        }
        let i = self.edges.len();
        self.held.entry(edge.item.clone()).or_default().push(i);
        self.naming
            .entry(edge.depends_on.clone())
            .or_default()
            .push(i);
        self.edges.push(edge);
    }

    pub(crate) fn item_mut(&mut self, id: &str) -> Option<&mut WorkItem> {
        let i = *self.index.get(id)?;
        Some(&mut self.items[i])
    }

    pub(crate) fn set_status(
        &mut self,
        id: &str,
        to: Status,
        origin: Option<WorkItemId>,
        now: DateTime<Utc>,
    ) {
        if let Some(item) = self.item_mut(id) {
            item.status = to;
            item.status_origin = origin;
            item.updated_at = now;
            if to.is_closed() && item.closed_at.is_none() {
                item.closed_at = Some(now);
            }
        }
    }

    pub(crate) fn repoint(&mut self, item: &str, link: Link, old: &str, new: &str) {
        match link {
            Link::Edge(kind) => {
                let Some(pos) = self
                    .edges
                    .iter()
                    .position(|e| e.item == item && e.depends_on == old && e.kind == kind)
                else {
                    return;
                };
                let moved = Edge {
                    item: item.to_string(),
                    depends_on: new.to_string(),
                    kind,
                };
                if self.edges.contains(&moved) {
                    self.edges.remove(pos);
                } else {
                    self.edges[pos] = moved;
                }
                self.reindex_edges();
            }
            Link::Input => {
                let Some(row) = self.item_mut(item) else {
                    return;
                };
                let mut inputs: Vec<WorkItemId> = Vec::with_capacity(row.inputs_from.len());
                for entry in row.inputs_from.drain(..) {
                    let entry = if entry == old { new.to_string() } else { entry };
                    if !inputs.contains(&entry) {
                        inputs.push(entry);
                    }
                }
                row.inputs_from = inputs;
            }
        }
    }

    pub(crate) fn remove_edge(&mut self, edge: &Edge) {
        if let Some(pos) = self.edges.iter().position(|e| e == edge) {
            self.edges.remove(pos);
            self.reindex_edges();
        }
    }

    fn reindex_children(&mut self) {
        self.children.clear();
        for item in &self.items {
            if let Some(parent) = &item.parent {
                self.children
                    .entry(parent.clone())
                    .or_default()
                    .push(item.id.clone());
            }
        }
    }

    fn reindex_edges(&mut self) {
        self.held.clear();
        self.naming.clear();
        for (i, edge) in self.edges.iter().enumerate() {
            self.held.entry(edge.item.clone()).or_default().push(i);
            self.naming
                .entry(edge.depends_on.clone())
                .or_default()
                .push(i);
        }
    }
}
