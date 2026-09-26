//! The dependency graph behind the cycle check and "ordered after" (plan
//! sections 4.3, 4.4 and 14.1).
//!
//! An arc `x -> y` reads "x depends on y". For the check (4.4):
//!
//! - every edge row is an arc from its `item` to its `depends_on` (ordering
//!   edges only, or every kind for the cycle check);
//! - a parent depends on each of its children;
//! - each item inherits its ancestors' ordering edges.

use std::collections::{HashMap, HashSet, VecDeque};

use rustykrab_core::work::{Edge, WorkItemId};

use super::Snapshot;

pub(crate) struct Arcs {
    pub(crate) nodes: Vec<WorkItemId>,
    index: HashMap<WorkItemId, usize>,
    out: Vec<Vec<usize>>,
}

impl Arcs {
    /// Build the arcs over every item in `snap`. `all_kinds` adds the
    /// history edges (`supersedes`, `discovered_from`); `skip` leaves out
    /// edges already reported.
    pub(crate) fn build(snap: &Snapshot, all_kinds: bool, skip: &HashSet<Edge>) -> Arcs {
        let nodes: Vec<WorkItemId> = snap.items().iter().map(|i| i.id.clone()).collect();
        let index: HashMap<WorkItemId, usize> = nodes
            .iter()
            .enumerate()
            .map(|(i, id)| (id.clone(), i))
            .collect();
        let mut out: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
        let mut add = |from: usize, to: usize| {
            if !out[from].contains(&to) {
                out[from].push(to);
            }
        };
        for edge in snap.edges() {
            if skip.contains(edge) || !(all_kinds || edge.kind.is_ordering()) {
                continue;
            }
            if let (Some(&from), Some(&to)) = (index.get(&edge.item), index.get(&edge.depends_on)) {
                add(from, to);
            }
        }
        for (i, item) in snap.items().iter().enumerate() {
            if let Some(&p) = item.parent.as_ref().and_then(|p| index.get(p)) {
                add(p, i);
            }
            for ancestor in snap.ancestors(&item.id) {
                for edge in snap.edges_held_by(&ancestor) {
                    if !edge.kind.is_ordering() || skip.contains(edge) {
                        continue;
                    }
                    if let Some(&to) = index.get(&edge.depends_on) {
                        add(i, to);
                    }
                }
            }
        }
        Arcs { nodes, index, out }
    }

    /// Every item `id` depends on, directly, transitively, through an
    /// ancestor's edges or through a parent's children. Excludes `id`
    /// unless it sits on a cycle.
    pub(crate) fn reach(&self, id: &str) -> HashSet<WorkItemId> {
        let Some(&start) = self.index.get(id) else {
            return HashSet::new();
        };
        let mut seen = vec![false; self.nodes.len()];
        let mut queue: VecDeque<usize> = self.out[start].iter().copied().collect();
        let mut found = HashSet::new();
        while let Some(n) = queue.pop_front() {
            if seen[n] {
                continue;
            }
            seen[n] = true;
            found.insert(self.nodes[n].clone());
            queue.extend(self.out[n].iter().copied());
        }
        found
    }

    /// The cycles: strongly connected components with more than one node,
    /// or one node with an arc to itself. Each comes as its node indexes in
    /// row order.
    pub(crate) fn cycles(&self) -> Vec<Vec<usize>> {
        strongly_connected(&self.out)
            .into_iter()
            .filter(|c| c.len() > 1 || self.out[c[0]].contains(&c[0]))
            .map(|mut c| {
                c.sort_unstable();
                c
            })
            .collect()
    }

    /// One concrete cycle through the component, starting and ending at its
    /// first node, for the rejection's detail.
    pub(crate) fn cycle_path(&self, component: &[usize]) -> Vec<WorkItemId> {
        let members: HashSet<usize> = component.iter().copied().collect();
        let start = component[0];
        let mut prev: HashMap<usize, usize> = HashMap::new();
        let mut queue = VecDeque::from([start]);
        let mut seen = HashSet::from([start]);
        let mut last = None;
        'search: while let Some(n) = queue.pop_front() {
            for &m in &self.out[n] {
                if m == start {
                    last = Some(n);
                    break 'search;
                }
                if members.contains(&m) && seen.insert(m) {
                    prev.insert(m, n);
                    queue.push_back(m);
                }
            }
        }
        let mut path = vec![self.nodes[start].clone()];
        let mut back = Vec::new();
        let mut at = last;
        while let Some(n) = at {
            if n == start {
                break;
            }
            back.push(self.nodes[n].clone());
            at = prev.get(&n).copied();
        }
        back.reverse();
        path.extend(back);
        path.push(self.nodes[start].clone());
        path
    }
}

/// Tarjan's algorithm, iterative so a deep chain cannot overflow the stack.
fn strongly_connected(out: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let n = out.len();
    let unvisited = usize::MAX;
    let mut index = vec![unvisited; n];
    let mut low = vec![0; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut next = 0;
    let mut components = Vec::new();
    for root in 0..n {
        if index[root] != unvisited {
            continue;
        }
        index[root] = next;
        low[root] = next;
        next += 1;
        stack.push(root);
        on_stack[root] = true;
        let mut calls: Vec<(usize, usize)> = vec![(root, 0)];
        while let Some(frame) = calls.last_mut() {
            let v = frame.0;
            if frame.1 < out[v].len() {
                let w = out[v][frame.1];
                frame.1 += 1;
                if index[w] == unvisited {
                    index[w] = next;
                    low[w] = next;
                    next += 1;
                    stack.push(w);
                    on_stack[w] = true;
                    calls.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
            } else {
                calls.pop();
                if let Some(&(u, _)) = calls.last() {
                    low[u] = low[u].min(low[v]);
                }
                if low[v] == index[v] {
                    let mut component = Vec::new();
                    while let Some(w) = stack.pop() {
                        on_stack[w] = false;
                        component.push(w);
                        if w == v {
                            break;
                        }
                    }
                    components.push(component);
                }
            }
        }
    }
    components
}
