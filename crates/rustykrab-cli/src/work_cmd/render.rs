//! Pure rendering for `rustykrab work` (plan section 14.2). Each function
//! takes a reply the daemon sent and returns the text to print, so every
//! shape is tested on fixtures without a daemon.
//!
//! One line per item: `#id title`, kind, status, then a note. A leaf's
//! cascade status carries `<- #origin`; a parent shows its roll-up with
//! `done/total done` and the origin of what holds it; a failed item names
//! the rungs it climbed. In a tree, edges and `inputs_from` follow on a
//! continuation line in words, and an archived item is its one-line
//! summary.

use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, Utc};

use rustykrab_control::handle::{GraphNode, GraphView, TickReport};
use rustykrab_core::work::{
    Budget, CancelReason, Edge, EdgeKind, Rung, Status, Trigger, WorkItem, WorkItemId,
};
use rustykrab_gateway::work_routes::{ApproveReply, CancelReply, ItemDetail, PlanPreview, WorkRow};
use rustykrab_store::ArchivedItem;

/// The widest bare status, `verifying`. A status with a reason overflows
/// the column, and its note follows after one space.
const STATUS_WIDTH: usize = 9;
const TITLE_MAX: usize = 60;
const SHORT_ID: usize = 8;

/// `#` and the id's first eight characters.
pub(super) fn tag(id: &str) -> String {
    let short: String = id.chars().take(SHORT_ID).collect();
    format!("#{short}")
}

fn tags<'a>(ids: impl IntoIterator<Item = &'a WorkItemId>) -> String {
    ids.into_iter()
        .map(|id| tag(id))
        .collect::<Vec<_>>()
        .join(" ")
}

fn title(raw: &str) -> String {
    let flat = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= TITLE_MAX {
        return flat;
    }
    let cut: String = flat.chars().take(TITLE_MAX - 3).collect();
    format!("{}...", cut.trim_end())
}

/// A roll-up held by a cascade shows the bare status: the origin beside it
/// already says why.
fn parent_status(status: &Status) -> String {
    let cascade = matches!(status, Status::Blocked(r) if r.is_cascade())
        || *status == Status::Cancelled(CancelReason::Cascade);
    if cascade {
        status.name().to_string()
    } else {
        status.to_string()
    }
}

/// `1/4 done; origin #43`
fn rollup_note(done: u32, total: u32, origin: Option<&WorkItemId>) -> String {
    let mut note = format!("{done}/{total} done");
    if let Some(origin) = origin {
        note.push_str(&format!("; origin {}", tag(origin)));
    }
    note
}

/// `<- #43` beside a leaf's cascade status.
fn origin_note(item: &WorkItem) -> Option<String> {
    item.status_origin
        .as_ref()
        .map(|origin| format!("<- {}", tag(origin)))
}

fn rung_label(rung: Rung) -> &'static str {
    match rung {
        Rung::SwitchWorker => "switch worker",
        Rung::PlanB => "plan B",
        other => other.as_str(),
    }
}

fn edge_verb(kind: EdgeKind) -> &'static str {
    match kind {
        EdgeKind::Blocks => "blocked by",
        EdgeKind::WaitsFor => "waits for",
        EdgeKind::ConditionalOnFailure => "plan B for",
        EdgeKind::Supersedes => "supersedes",
        EdgeKind::DiscoveredFrom => "discovered from",
    }
}

/// Edges in words: `blocked by #42 #43; inputs from #42 #43`.
fn edge_words<'a>(
    edges: impl IntoIterator<Item = &'a Edge>,
    inputs: &[WorkItemId],
) -> Option<String> {
    let edges: Vec<&Edge> = edges.into_iter().collect();
    let mut parts = Vec::new();
    for kind in EdgeKind::ALL {
        let upstreams: Vec<&WorkItemId> = edges
            .iter()
            .filter(|e| e.kind == kind)
            .map(|e| &e.depends_on)
            .collect();
        if !upstreams.is_empty() {
            parts.push(format!("{} {}", edge_verb(kind), tags(upstreams)));
        }
    }
    if !inputs.is_empty() {
        parts.push(format!("inputs from {}", tags(inputs)));
    }
    (!parts.is_empty()).then(|| parts.join("; "))
}

fn tokens(n: u64) -> String {
    let scaled = |value: f64, unit: &str| {
        let text = format!("{value:.1}");
        format!("{}{unit}", text.trim_end_matches(".0"))
    };
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => scaled(n as f64 / 1_000.0, "k"),
        _ => scaled(n as f64 / 1_000_000.0, "M"),
    }
}

fn wall(seconds: u64) -> String {
    let (h, m, s) = (seconds / 3_600, seconds % 3_600 / 60, seconds % 60);
    let mut text = String::new();
    if h > 0 {
        text.push_str(&format!("{h}h"));
    }
    if m > 0 {
        text.push_str(&format!("{m}m"));
    }
    if s > 0 || text.is_empty() {
        text.push_str(&format!("{s}s"));
    }
    text
}

/// What a budget spends, or a sum of them: tokens, iterations, wall time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Spend {
    tokens: u64,
    iterations: u64,
    wall_seconds: u64,
}

impl Spend {
    fn of(budget: &Budget) -> Spend {
        Spend {
            tokens: budget.tokens,
            iterations: u64::from(budget.iterations),
            wall_seconds: budget.wall_seconds,
        }
    }

    fn add(self, other: Spend) -> Spend {
        Spend {
            tokens: self.tokens.saturating_add(other.tokens),
            iterations: self.iterations.saturating_add(other.iterations),
            wall_seconds: self.wall_seconds.saturating_add(other.wall_seconds),
        }
    }

    fn exceeds(&self, envelope: &Spend) -> bool {
        self.tokens > envelope.tokens
            || self.iterations > envelope.iterations
            || self.wall_seconds > envelope.wall_seconds
    }

    fn text(&self) -> String {
        format!(
            "{} tokens, {} iterations, {}",
            tokens(self.tokens),
            self.iterations,
            wall(self.wall_seconds)
        )
    }
}

fn when(at: &DateTime<Utc>) -> String {
    at.format("%Y-%m-%d %H:%M UTC").to_string()
}

fn trigger(trigger: &Trigger) -> String {
    match trigger {
        Trigger::Now => "now".to_string(),
        Trigger::At(at) => format!("at {}", when(at)),
        Trigger::OnCredential(name) => format!("on credential {name}"),
        Trigger::OnMcp(server) => format!("on MCP server {server}"),
        Trigger::OnAnswer(question) => format!("on answer {question}"),
    }
}

// ── layout ─────────────────────────────────────────────────────────────

/// One item's line and what follows it.
struct Row {
    /// Tree glyphs, `#id` and the title; for an archived item, the whole
    /// line.
    label: String,
    kind: &'static str,
    status: String,
    note: String,
    /// Continuation lines, printed as given.
    more: Vec<String>,
    archived: bool,
}

/// Columns: the label padded two past the widest, the kind likewise, the
/// status in its fixed column, then the note. Trailing space is trimmed.
fn lay_out(rows: &[Row]) -> String {
    let live = || rows.iter().filter(|r| !r.archived);
    let label_width = live().map(|r| r.label.chars().count()).max().unwrap_or(0) + 2;
    let kind_width = live().map(|r| r.kind.len()).max().unwrap_or(0) + 2;
    let mut out = String::new();
    for row in rows {
        if row.archived {
            push_line(&mut out, &row.label);
        } else {
            let mut line = pad(&row.label, label_width);
            line.push_str(&pad(row.kind, kind_width));
            line.push_str(&row.status);
            if !row.note.is_empty() {
                let gap = STATUS_WIDTH.saturating_sub(row.status.chars().count()) + 1;
                line.push_str(&" ".repeat(gap));
                line.push_str(&row.note);
            }
            push_line(&mut out, &line);
        }
        for more in &row.more {
            push_line(&mut out, more);
        }
    }
    out
}

fn pad(text: &str, width: usize) -> String {
    let fill = width.saturating_sub(text.chars().count());
    format!("{text}{}", " ".repeat(fill))
}

fn push_line(out: &mut String, line: &str) {
    out.push_str(line.trim_end());
    out.push('\n');
}

fn field(out: &mut String, name: &str, value: impl AsRef<str>) {
    let value = value.as_ref();
    if !value.is_empty() {
        out.push_str(&format!("  {name:<10} {value}\n"));
    }
}

// ── trees ──────────────────────────────────────────────────────────────

/// `work show <id> --graph`: the tree by parent (plan section 14.2).
pub(super) fn tree(view: &GraphView, rungs: &BTreeMap<WorkItemId, Vec<Rung>>) -> String {
    lay_out(&tree_rows(view, rungs, |_| None))
}

/// The rows of a depth-first [`GraphView`]. `detail` adds one more
/// continuation line under an item.
fn tree_rows(
    view: &GraphView,
    rungs: &BTreeMap<WorkItemId, Vec<Rung>>,
    detail: impl Fn(&GraphNode) -> Option<String>,
) -> Vec<Row> {
    let nodes = &view.nodes;
    // Whether the latest item seen at each depth is the last of its
    // siblings: its column carries a bar only while more siblings follow.
    let mut last_at: Vec<bool> = Vec::new();
    let mut rows = Vec::with_capacity(nodes.len());
    for (i, node) in nodes.iter().enumerate() {
        let depth = node.depth as usize;
        let is_last = nodes[i + 1..]
            .iter()
            .find(|n| n.depth <= node.depth)
            .is_none_or(|n| n.depth < node.depth);
        let has_children = nodes.get(i + 1).is_some_and(|n| n.depth > node.depth);
        last_at.resize(depth, true);
        let lead: String = last_at
            .iter()
            .skip(1)
            .map(|&last| if last { "   " } else { "│  " })
            .collect();
        last_at.push(is_last);
        let (glyph, own) = match (depth, is_last) {
            (0, _) => ("", ""),
            (_, true) => ("└─ ", "   "),
            (_, false) => ("├─ ", "│  "),
        };
        let id = tag(&node.item.id);
        let bar = if has_children { "│" } else { " " };
        let continuation = format!("{lead}{own}{bar}{}", " ".repeat(id.chars().count()));

        if let Some(summary) = &node.archived_summary {
            rows.push(Row {
                label: format!("{lead}{glyph}{id} {summary}"),
                kind: "",
                status: String::new(),
                note: String::new(),
                more: Vec::new(),
                archived: true,
            });
            continue;
        }

        let item = &node.item;
        let is_parent = node.rollup.is_some() || node.children_total > 0;
        let (status, note) = if is_parent {
            let rolled = node.rollup.unwrap_or(item.status);
            (
                parent_status(&rolled),
                rollup_note(
                    node.children_done,
                    node.children_total,
                    item.status_origin.as_ref(),
                ),
            )
        } else {
            let mut notes: Vec<String> = origin_note(item).into_iter().collect();
            let stuck = matches!(item.status, Status::Failed)
                || matches!(item.status, Status::Blocked(r) if !r.is_cascade());
            if let Some(climbed) = rungs.get(&item.id).filter(|r| stuck && !r.is_empty()) {
                let labels: Vec<&str> = climbed.iter().map(|r| rung_label(*r)).collect();
                notes.push(format!("rungs: {}", labels.join(", ")));
            }
            (item.status.to_string(), notes.join("; "))
        };
        let mut more = Vec::new();
        if let Some(words) = edge_words(&node.edges, &item.inputs_from) {
            more.push(format!("{continuation}{words}"));
        }
        if let Some(extra) = detail(node) {
            more.push(format!("{continuation}{extra}"));
        }
        rows.push(Row {
            label: format!("{lead}{glyph}{id} {}", title(&item.title)),
            kind: item.kind.as_str(),
            status,
            note,
            more,
            archived: false,
        });
    }
    rows
}

// ── lists and items ────────────────────────────────────────────────────

/// `work list` and `work ready`: one line per item, a parent as its
/// roll-up. With `fold`, an item whose parent is listed folds into the
/// parent's line.
pub(super) fn list(rows: &[WorkRow], fold: bool, empty: &str) -> String {
    let listed: HashSet<&str> = rows.iter().map(|r| r.item.id.as_str()).collect();
    let lines: Vec<Row> = rows
        .iter()
        .filter(|r| !fold || r.item.parent.as_deref().is_none_or(|p| !listed.contains(p)))
        .map(|r| {
            let (status, note) = match &r.rollup {
                Some(rollup) => (
                    parent_status(&rollup.status),
                    rollup_note(
                        rollup.children_done,
                        rollup.children_total,
                        r.item.status_origin.as_ref(),
                    ),
                ),
                None => (
                    r.item.status.to_string(),
                    origin_note(&r.item).unwrap_or_default(),
                ),
            };
            Row {
                label: format!("{} {}", tag(&r.item.id), title(&r.item.title)),
                kind: r.item.kind.as_str(),
                status,
                note,
                more: Vec::new(),
                archived: false,
            }
        })
        .collect();
    if lines.is_empty() {
        return format!("{empty}\n");
    }
    lay_out(&lines)
}

/// `work show <id>`: one item's fields.
pub(super) fn detail(d: &ItemDetail) -> String {
    let item = &d.item;
    let mut out = format!("{} {}\n", tag(&item.id), item.title);
    field(&mut out, "kind", item.kind.as_str());
    let mut status = item.status.to_string();
    if let Some(note) = origin_note(item) {
        status = format!("{status} {note}");
    }
    field(&mut out, "status", status);
    if let Some(r) = &d.rollup {
        field(
            &mut out,
            "roll-up",
            format!(
                "{}, {}",
                r.status,
                rollup_note(r.children_done, r.children_total, None)
            ),
        );
    }
    if let Some(parent) = &item.parent {
        field(&mut out, "parent", tag(parent));
    }
    field(&mut out, "objective", &item.objective);
    field(&mut out, "done when", &item.done_when);
    let edges = d.edges.iter().map(|e| &e.edge);
    field(
        &mut out,
        "edges",
        edge_words(edges, &item.inputs_from).unwrap_or_default(),
    );
    for e in d.edges.iter().filter(|e| e.archived.is_some()) {
        let summary = e.archived.as_deref().unwrap_or_default();
        field(
            &mut out,
            "archived",
            format!("{} {summary}", tag(&e.edge.depends_on)),
        );
    }
    let dependents: Vec<String> = d
        .dependents
        .iter()
        .map(|e| format!("{} ({})", tag(&e.item), e.kind.as_str()))
        .collect();
    field(&mut out, "needed by", dependents.join(", "));
    field(&mut out, "trigger", trigger(&item.trigger));
    if let Some(at) = &item.expires_at {
        field(&mut out, "expires", when(at));
    }
    field(&mut out, "worker", item.worker_kind.as_str());
    field(&mut out, "writes", item.writable_resources.join(", "));
    field(&mut out, "tools", item.required_tools.join(", "));
    field(&mut out, "budget", Spend::of(&item.budget).text());
    if let Some(lease) = &d.lease {
        field(
            &mut out,
            "leased to",
            format!("{} since {}", lease.worker, when(&lease.since)),
        );
    }
    let rungs: Vec<&str> = d.ladder.iter().map(|r| r.rung.as_str()).collect();
    field(&mut out, "rungs", rungs.join(", "));
    if let Some(error) = &d.last_error {
        field(
            &mut out,
            "last error",
            format!(
                "{}/{}: {}",
                error.class.as_str(),
                error.subclass.as_str(),
                error.detail
            ),
        );
    }
    field(
        &mut out,
        "history",
        format!("{} events, {} evidence", d.events.len(), d.evidence.len()),
    );
    out
}

// ── plans and commands ─────────────────────────────────────────────────

/// `work plan <id>`: the tree with each item's terms, the budget total
/// against the root's, the rationale and the policy (plan section 14.2).
pub(super) fn plan(p: &PlanPreview) -> String {
    let root_id = &p.graph.root;
    let root = p.graph.nodes.iter().find(|n| &n.item.id == root_id);
    let root_title = root.map(|n| n.item.title.as_str()).unwrap_or_default();
    let mut out = if p.held.is_empty() {
        format!("Plan under {} {root_title} (nothing held)\n", tag(root_id))
    } else {
        format!(
            "Plan awaiting approval under {} {root_title}\n",
            tag(root_id)
        )
    };
    if !p.plan.rationale.trim().is_empty() {
        out.push_str(&format!("Rationale: {}\n", p.plan.rationale.trim()));
    }
    let mut policy = format!("Policy: {}", p.plan.policy.as_deref().unwrap_or("none"));
    if !p.held.is_empty() {
        policy.push_str(&format!("; held: {}", tags(&p.held)));
    }
    out.push_str(&policy);
    out.push('\n');

    let held: HashSet<&str> = p.held.iter().map(String::as_str).collect();
    let rows = tree_rows(&p.graph, &BTreeMap::new(), |node| {
        (&node.item.id != root_id).then(|| terms(&node.item, held.contains(node.item.id.as_str())))
    });
    out.push_str(&lay_out(&rows));

    let children: Vec<&GraphNode> = p
        .graph
        .nodes
        .iter()
        .filter(|n| n.depth == 1 && n.archived_summary.is_none())
        .collect();
    let total = children.iter().fold(Spend::default(), |sum, n| {
        sum.add(Spend::of(&n.item.budget))
    });
    let mut budget = format!("Budget: {} across {} items", total.text(), children.len());
    if let Some(root) = root {
        let envelope = Spend::of(&root.item.budget);
        budget.push_str(&format!("; {} allows {}", tag(root_id), envelope.text()));
        if total.exceeds(&envelope) {
            budget.push_str(" (over)");
        }
    }
    out.push_str(&budget);
    out.push('\n');
    if !p.held.is_empty() {
        out.push_str(&format!(
            "Approve with `rustykrab work approve {id}`, or decline with `rustykrab work reject {id} [reason]`.\n",
            id = tag(root_id)
        ));
    }
    out
}

/// An item's terms in a plan preview: budget, worker kind, trigger,
/// writable resources, and whether approval holds it.
fn terms(item: &WorkItem, held: bool) -> String {
    let mut terms = vec![
        format!("budget {}", Spend::of(&item.budget).text()),
        format!("worker {}", item.worker_kind.as_str()),
        format!("trigger {}", trigger(&item.trigger)),
    ];
    if !item.writable_resources.is_empty() {
        terms.push(format!("writes {}", item.writable_resources.join(", ")));
    }
    if held {
        terms.push("held".to_string());
    }
    terms.join("; ")
}

pub(super) fn approved(r: &ApproveReply) -> String {
    if r.released.is_empty() {
        format!(
            "approved the plan under {}; nothing was held\n",
            tag(&r.root)
        )
    } else {
        format!(
            "approved the plan under {}; released {}\n",
            tag(&r.root),
            tags(&r.released)
        )
    }
}

pub(super) fn rejected(r: &CancelReply) -> String {
    let mut out = if r.cancelled.is_empty() {
        format!(
            "rejected the plan under {}; nothing was held\n",
            tag(&r.item)
        )
    } else {
        format!(
            "rejected the plan under {}; cancelled {}\n",
            tag(&r.item),
            tags(&r.cancelled)
        )
    };
    out.push_str(&finished(r));
    out
}

/// `work cancel`: what was cancelled and what had already finished
/// (plan section 14.2).
pub(super) fn cancelled(r: &CancelReply) -> String {
    let mut out = if r.cancelled.is_empty() {
        format!("nothing under {} was open\n", tag(&r.item))
    } else {
        format!("cancelled {}\n", tags(&r.cancelled))
    };
    out.push_str(&finished(r));
    out
}

fn finished(r: &CancelReply) -> String {
    if r.already_finished.is_empty() {
        return String::new();
    }
    let items: Vec<String> = r
        .already_finished
        .iter()
        .map(|f| format!("{} {} ({})", tag(&f.id), title(&f.title), f.status))
        .collect();
    format!("already finished: {}\n", items.join(", "))
}

// ── the archive and the loop ───────────────────────────────────────────

/// `work archive list | search`: one line per compacted item.
pub(super) fn archive_list(items: &[ArchivedItem]) -> String {
    if items.is_empty() {
        return "no archived items\n".to_string();
    }
    items
        .iter()
        .map(|a| {
            format!(
                "{}  {}  {}\n",
                tag(&a.id),
                a.closed_at.format("%Y-%m-%d"),
                a.summary
            )
        })
        .collect()
}

/// `work archive show <id>`: what compaction kept.
pub(super) fn archived(a: &ArchivedItem) -> String {
    let mut out = format!(
        "{} {} (archived {})\n",
        tag(&a.id),
        a.title,
        a.archived_at.format("%Y-%m-%d")
    );
    field(&mut out, "kind", a.kind.as_str());
    field(&mut out, "status", a.status.to_string());
    field(&mut out, "closed", when(&a.closed_at));
    if let Some(parent) = &a.parent {
        field(&mut out, "parent", tag(parent));
    }
    field(&mut out, "worker", a.worker.as_deref().unwrap_or_default());
    field(
        &mut out,
        "edges",
        edge_words(&a.edges, &[]).unwrap_or_default(),
    );
    field(&mut out, "summary", &a.summary);
    out
}

/// `work tick`: what one pass of the loop did.
pub(super) fn tick(r: &TickReport) -> String {
    let mut out = format!(
        "tick: {} transitions, {} notices\n",
        r.transitions, r.notices
    );
    for (name, ids) in [
        ("made ready", &r.made_ready),
        ("leased", &r.leased),
        ("reconciled", &r.reconciled),
        ("expired", &r.expired),
        ("archived", &r.archived),
    ] {
        if !ids.is_empty() {
            out.push_str(&format!("  {name}: {}\n", tags(ids)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use rustykrab_core::work::{
        BlockedReason, EventKind, Evidence, RungEvent, WorkEvent, WorkKind, WorkerKind,
    };
    use rustykrab_gateway::work_routes::{EdgeView, FinishedItem, GraphReply, RollupView};
    use rustykrab_store::WorkPlanRow;

    fn at(day: u32) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(&format!("2026-09-{day:02}T10:00:00Z"))
            .unwrap()
            .with_timezone(&Utc)
    }

    fn item(id: &str, kind: WorkKind, title: &str, status: Status) -> WorkItem {
        WorkItem {
            id: id.into(),
            kind,
            title: title.into(),
            objective: "objective".into(),
            done_when: "done".into(),
            constraints: vec![],
            decisions_made: vec![],
            artifact_refs: vec![],
            required_tools: vec![],
            required_mcp_servers: vec![],
            worker_kind: WorkerKind::Any,
            writable_resources: vec![],
            parent: None,
            inputs_from: vec![],
            origin_conversation_id: None,
            trigger: Trigger::Now,
            preconditions: vec![],
            expires_at: None,
            budget: Budget::default(),
            priority: 0,
            status,
            status_origin: None,
            plan_id: None,
            held_by: None,
            created_at: at(1),
            updated_at: at(1),
            closed_at: None,
        }
    }

    fn origin(mut item: WorkItem, origin: &str) -> WorkItem {
        item.status_origin = Some(origin.into());
        item
    }

    fn blocks(item: &str, upstream: &str) -> Edge {
        Edge {
            item: item.into(),
            depends_on: upstream.into(),
            kind: EdgeKind::Blocks,
        }
    }

    fn node(item: WorkItem, depth: u32) -> GraphNode {
        GraphNode {
            item,
            depth,
            edges: vec![],
            rollup: None,
            children_done: 0,
            children_total: 0,
            archived_summary: None,
        }
    }

    fn parent(mut node: GraphNode, done: u32, total: u32) -> GraphNode {
        node.rollup = Some(node.item.status);
        node.children_done = done;
        node.children_total = total;
        node
    }

    /// Plan section 14.2's example, as the controller would return it.
    fn lisbon() -> (GraphView, BTreeMap<WorkItemId, Vec<Rung>>) {
        use WorkKind::{Personal, Research};
        let held = Status::Blocked(BlockedReason::UpstreamFailed);
        let mut book = node(
            origin(item("44", Personal, "Book flight and hotel", held), "43"),
            1,
        );
        book.edges = vec![blocks("44", "42"), blocks("44", "43")];
        book.item.inputs_from = vec!["42".into(), "43".into()];
        let mut calendar = node(
            origin(
                item("45", Personal, "Add the trip to the calendar", held),
                "43",
            ),
            1,
        );
        calendar.edges = vec![blocks("45", "44")];
        let view = GraphView {
            root: "41".into(),
            nodes: vec![
                parent(
                    node(
                        origin(item("41", Personal, "Plan the Lisbon trip", held), "43"),
                        0,
                    ),
                    1,
                    4,
                ),
                node(item("42", Research, "Find flight options", Status::Done), 1),
                node(
                    item("43", Research, "Find hotels near the venue", Status::Failed),
                    1,
                ),
                book,
                calendar,
            ],
        };
        let rungs = BTreeMap::from([(
            "43".to_string(),
            vec![Rung::Retry, Rung::Repair, Rung::SwitchWorker],
        )]);
        (view, rungs)
    }

    #[test]
    fn the_14_2_example_renders_verbatim() {
        let (view, rungs) = lisbon();
        let expected = "\
#41 Plan the Lisbon trip             personal  blocked   1/4 done; origin #43
├─ #42 Find flight options           research  done
├─ #43 Find hotels near the venue    research  failed    rungs: retry, repair, switch worker
├─ #44 Book flight and hotel         personal  blocked(upstream_failed) <- #43
│      blocked by #42 #43; inputs from #42 #43
└─ #45 Add the trip to the calendar  personal  blocked(upstream_failed) <- #43
       blocked by #44
";
        assert_eq!(tree(&view, &rungs), expected);
    }

    #[test]
    fn the_graph_reply_survives_the_wire() {
        let (graph, rungs) = lisbon();
        let reply = GraphReply { graph, rungs };
        let wire = serde_json::to_value(&reply).unwrap();
        assert_eq!(wire["rungs"]["43"][2], "switch_worker");
        assert_eq!(wire["nodes"][3]["item"]["status_origin"], "43");
        let back: GraphReply = serde_json::from_value(wire).unwrap();
        assert_eq!(back, reply);
    }

    #[test]
    fn deeper_trees_carry_their_bars_and_archived_lines() {
        use WorkKind::{Personal, Research};
        let mut phase = parent(node(item("b", Personal, "Book", Status::Running), 1), 0, 2);
        phase.rollup = Some(Status::Running);
        let mut hotel = node(item("b2", Personal, "Hotel", Status::Ready), 2);
        hotel.edges = vec![blocks("b2", "b1")];
        let mut archived = node(item("old", Research, "Old", Status::Done), 1);
        archived.archived_summary = Some("Old research  research  done".into());
        let mut root = parent(node(item("p", Personal, "Trip", Status::Running), 0), 0, 3);
        root.edges = vec![Edge {
            item: "p".into(),
            depends_on: "q".into(),
            kind: EdgeKind::WaitsFor,
        }];
        let view = GraphView {
            root: "p".into(),
            nodes: vec![
                root,
                phase,
                node(item("b1", Personal, "Flight", Status::Done), 2),
                hotel,
                archived,
                node(item("c", Research, "Calendar", Status::Queued), 1),
            ],
        };
        let expected = "\
#p Trip           personal  running   0/3 done
│  waits for #q
├─ #b Book        personal  running   0/2 done
│  ├─ #b1 Flight  personal  done
│  └─ #b2 Hotel   personal  ready
│         blocked by #b1
├─ #old Old research  research  done
└─ #c Calendar    research  queued
";
        assert_eq!(tree(&view, &BTreeMap::new()), expected);
    }

    #[test]
    fn a_list_folds_children_into_their_parent_line() {
        let (view, _) = lisbon();
        let rows: Vec<WorkRow> = view
            .nodes
            .iter()
            .map(|n| {
                let mut item = n.item.clone();
                if n.depth == 1 {
                    item.parent = Some("41".into());
                }
                WorkRow {
                    rollup: n.rollup.map(|status| RollupView {
                        status,
                        children_done: n.children_done,
                        children_total: n.children_total,
                    }),
                    item,
                }
            })
            .chain([WorkRow {
                item: item(
                    "7c1d9e20-aaaa-4bbb-8ccc-000000000000",
                    WorkKind::Personal,
                    "Renew the library card",
                    Status::Ready,
                ),
                rollup: None,
            }])
            .collect();
        assert_eq!(
            list(&rows, true, "empty"),
            "\
#41 Plan the Lisbon trip          personal  blocked   1/4 done; origin #43
#7c1d9e20 Renew the library card  personal  ready
"
        );
        let unfolded = list(&rows[3..5], true, "empty");
        assert_eq!(
            unfolded,
            "\
#44 Book flight and hotel         personal  blocked(upstream_failed) <- #43
#45 Add the trip to the calendar  personal  blocked(upstream_failed) <- #43
"
        );
        assert_eq!(list(&rows, false, "empty").lines().count(), 6);
        assert_eq!(list(&[], true, "nothing is ready"), "nothing is ready\n");
    }

    #[test]
    fn the_plan_preview_shows_terms_totals_and_policy() {
        use WorkKind::{Personal, Research};
        let consent = Status::Blocked(BlockedReason::NeedsConsent);
        let mut root = parent(
            node(item("t", Personal, "Switch the phone plan", consent), 0),
            0,
            2,
        );
        root.item.budget.tokens = 300_000;
        root.item.budget.iterations = 40;
        root.item.budget.wall_seconds = 7_200;
        let mut compare = node(
            item("x", Research, "Compare three phone plans", Status::Ready),
            1,
        );
        compare.item.budget.tokens = 150_000;
        compare.item.budget.iterations = 20;
        compare.item.budget.wall_seconds = 1_800;
        let mut switch = node(item("y", Personal, "Switch the carrier", consent), 1);
        switch.edges = vec![blocks("y", "x")];
        switch.item.writable_resources = vec!["carrier account".into()];
        switch.item.worker_kind = WorkerKind::Local;
        switch.item.trigger = Trigger::At(at(28));
        switch.item.budget.tokens = 200_000;
        switch.item.budget.iterations = 10;
        switch.item.budget.wall_seconds = 5_400;
        let preview = PlanPreview {
            plan: WorkPlanRow {
                id: "plan-1".into(),
                root: "t".into(),
                filed_by: None,
                rationale: "compare the plans, then switch".into(),
                approval_question: Some("q1".into()),
                policy: Some("approval.default".into()),
                created_at: at(26),
            },
            graph: GraphView {
                root: "t".into(),
                nodes: vec![root, compare, switch],
            },
            held: vec!["t".into(), "y".into()],
        };
        let expected = "\
Plan awaiting approval under #t Switch the phone plan
Rationale: compare the plans, then switch
Policy: approval.default; held: #t #y
#t Switch the phone plan         personal  blocked(needs_consent) 0/2 done
├─ #x Compare three phone plans  research  ready
│     budget 150k tokens, 20 iterations, 30m; worker any; trigger now
└─ #y Switch the carrier         personal  blocked(needs_consent)
      blocked by #x
      budget 200k tokens, 10 iterations, 1h30m; worker local; trigger at 2026-09-28 10:00 UTC; writes carrier account; held
Budget: 350k tokens, 30 iterations, 2h across 2 items; #t allows 300k tokens, 40 iterations, 2h (over)
Approve with `rustykrab work approve #t`, or decline with `rustykrab work reject #t [reason]`.
";
        assert_eq!(plan(&preview), expected);
    }

    #[test]
    fn commands_report_what_they_touched() {
        assert_eq!(
            approved(&ApproveReply {
                root: "t".into(),
                released: vec!["y".into()],
            }),
            "approved the plan under #t; released #y\n"
        );
        let reply = CancelReply {
            item: "41".into(),
            cancelled: vec!["41".into(), "44".into(), "45".into()],
            already_finished: vec![
                FinishedItem {
                    id: "42".into(),
                    title: "Find flight options".into(),
                    status: Status::Done,
                },
                FinishedItem {
                    id: "43".into(),
                    title: "Find hotels near the venue".into(),
                    status: Status::Failed,
                },
            ],
        };
        assert_eq!(
            cancelled(&reply),
            "cancelled #41 #44 #45\n\
             already finished: #42 Find flight options (done), #43 Find hotels near the venue (failed)\n"
        );
        assert_eq!(
            rejected(&CancelReply {
                item: "t".into(),
                cancelled: vec!["y".into()],
                already_finished: vec![],
            }),
            "rejected the plan under #t; cancelled #y\n"
        );
        assert_eq!(
            tick(&TickReport {
                made_ready: vec!["r".into()],
                archived: vec!["old".into()],
                transitions: 2,
                ..TickReport::default()
            }),
            "tick: 2 transitions, 0 notices\n  made ready: #r\n  archived: #old\n"
        );
    }

    #[test]
    fn an_item_shows_its_fields_and_archived_upstreams() {
        let mut book = origin(
            item(
                "44",
                WorkKind::Personal,
                "Book flight and hotel",
                Status::Blocked(BlockedReason::UpstreamFailed),
            ),
            "43",
        );
        book.parent = Some("41".into());
        book.inputs_from = vec!["42".into()];
        let detail_text = detail(&ItemDetail {
            item: book,
            edges: vec![
                EdgeView {
                    edge: blocks("44", "42"),
                    archived: None,
                },
                EdgeView {
                    edge: Edge {
                        item: "44".into(),
                        depends_on: "9".into(),
                        kind: EdgeKind::DiscoveredFrom,
                    },
                    archived: Some("Renew the passport  personal  done".into()),
                },
            ],
            dependents: vec![blocks("45", "44")],
            rollup: None,
            lease: None,
            ladder: vec![RungEvent {
                rung: Rung::Retry,
                at: at(2),
                error: None,
                outcome: "retry on pinch".into(),
            }],
            last_error: None,
            events: (0..3)
                .map(|n| WorkEvent {
                    item: "44".into(),
                    at: at(2 + n),
                    kind: EventKind::Transition,
                    from: None,
                    to: None,
                    actor: "controller".into(),
                    reason: None,
                    upstream: None,
                    origin: None,
                    evidence_ref: None,
                })
                .collect(),
            evidence: vec![Evidence {
                item: "44".into(),
                kind: "message".into(),
                reference: "m1".into(),
                hash: None,
                verified_by: None,
                at: at(3),
            }],
        });
        for line in [
            "#44 Book flight and hotel",
            "  status     blocked(upstream_failed) <- #43",
            "  parent     #41",
            "  edges      blocked by #42; discovered from #9; inputs from #42",
            "  archived   #9 Renew the passport  personal  done",
            "  needed by  #45 (blocks)",
            "  budget     200k tokens, 25 iterations, 1h",
            "  rungs      retry",
            "  history    3 events, 1 evidence",
        ] {
            assert!(
                detail_text.lines().any(|l| l == line),
                "missing `{line}` in:\n{detail_text}"
            );
        }
    }

    #[test]
    fn the_archive_prints_one_line_per_item() {
        let item = ArchivedItem {
            id: "0a1b2c3d-0000-4000-8000-000000000000".into(),
            kind: WorkKind::Personal,
            title: "Renew the passport".into(),
            parent: None,
            status: Status::Done,
            worker: Some("local".into()),
            cost: None,
            closed_at: at(1),
            archived_at: at(25),
            summary: "Renew the passport  personal  done; worker local".into(),
            edges: vec![],
        };
        assert_eq!(
            archive_list(std::slice::from_ref(&item)),
            "#0a1b2c3d  2026-09-01  Renew the passport  personal  done; worker local\n"
        );
        assert_eq!(archive_list(&[]), "no archived items\n");
        let shown = archived(&item);
        assert!(shown.starts_with("#0a1b2c3d Renew the passport (archived 2026-09-25)\n"));
        assert!(shown.contains("  worker     local\n"));
    }

    #[test]
    fn numbers_and_ids_print_compactly() {
        assert_eq!(tokens(950), "950");
        assert_eq!(tokens(200_000), "200k");
        assert_eq!(tokens(1_500), "1.5k");
        assert_eq!(tokens(2_000_000), "2M");
        assert_eq!(wall(3_600), "1h");
        assert_eq!(wall(5_400), "1h30m");
        assert_eq!(wall(90), "1m30s");
        assert_eq!(wall(0), "0s");
        assert_eq!(tag("3f2a9c1e-0000-4000-8000-000000000001"), "#3f2a9c1e");
        assert_eq!(tag("41"), "#41");
        assert_eq!(title(&"word ".repeat(30)).chars().count(), TITLE_MAX);
    }
}
