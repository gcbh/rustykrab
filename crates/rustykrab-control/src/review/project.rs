//! The projection rule and the projected fields (plan section 11), as pure
//! functions of an item, its neighbours and what is already projected.

use std::collections::HashMap;

use rustykrab_core::work::{
    CapabilityMode, Edge, EdgeKind, Evidence, Status, WorkFacets, WorkItem, WorkItemId, WorkKind,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// On every projected issue, so the adapter can list what it manages.
pub const LABEL_MANAGED: &str = "rustykrab";
/// A human's acceptance of a proposal.
pub const LABEL_ACCEPTED: &str = "rustykrab-accepted";
/// A human's decline of a proposal.
pub const LABEL_DECLINED: &str = "rustykrab-declined";

/// The fields a projection writes. Everything else on the issue belongs
/// to the humans, and only the decision vocabulary is read back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Projection {
    pub title: String,
    pub body: String,
    /// The labels the projection needs present: [`LABEL_MANAGED`], the
    /// kind's, and a proposal's review tier. Others are left alone.
    pub labels: Vec<String>,
    /// Open while the item is; closed once it closes.
    pub open: bool,
}

/// An item with what its issue shows beside it.
#[derive(Debug, Clone)]
pub struct ItemView<'a> {
    pub item: &'a WorkItem,
    /// The edges the item holds (it is the downstream).
    pub edges: &'a [Edge],
    pub evidence: &'a [Evidence],
    /// The worker that last held its lease.
    pub worker: Option<&'a str>,
    /// A parent's computed roll-up, `None` for a leaf.
    pub rollup: Option<Status>,
}

/// What a projection may say about other items.
#[derive(Debug, Clone, Default)]
pub struct ProjectionContext {
    /// Every live item by id, for the kind of a neighbour.
    pub items: HashMap<WorkItemId, WorkItem>,
    pub facets: HashMap<WorkItemId, WorkFacets>,
    /// The issue each projected item is on, by item.
    pub issues: HashMap<WorkItemId, String>,
}

impl ProjectionContext {
    /// How `id` appears in another item's issue: its issue when it has
    /// one, `rustykrab:#N` for a projectable item whose issue a later pass
    /// opens, else the opaque `local:#N`. A neighbour that is not
    /// projected, or is gone from the live rows, never shows its title,
    /// objective or evidence.
    fn reference(&self, id: &str) -> String {
        let projected = self
            .items
            .get(id)
            .is_some_and(|i| is_projectable(i, self.facets.get(id)));
        match self.issues.get(id) {
            Some(number) if projected => format!("#{number}"),
            None if projected => format!("rustykrab:#{}", short(id)),
            _ => local_ref(id),
        }
    }
}

fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

/// The opaque reference to a local-only item.
pub fn local_ref(id: &str) -> String {
    format!("local:#{}", short(id))
}

/// Section 11's rule: `code`, `proposal`, `internal` and a `capability`
/// build are projected; `personal`, `research`, and a capability
/// acquisition or request never are. A capability item that does not say
/// it is a build is treated as an acquisition, the conservative reading.
pub fn is_projectable(item: &WorkItem, facets: Option<&WorkFacets>) -> bool {
    match item.kind {
        WorkKind::Code | WorkKind::Proposal | WorkKind::Internal => true,
        WorkKind::Capability => facets.and_then(|f| f.capability) == Some(CapabilityMode::Build),
        WorkKind::Personal | WorkKind::Research => false,
    }
}

fn kind_label(item: &WorkItem) -> String {
    format!("rustykrab-{}", item.kind.as_str())
}

fn phrase(status: Status) -> String {
    match status.reason() {
        Some(r) => format!("{} ({r})", status.name()),
        None => status.name().to_string(),
    }
}

fn edge_words(kind: EdgeKind) -> &'static str {
    match kind {
        EdgeKind::Blocks => "blocked by",
        EdgeKind::WaitsFor => "waits for",
        EdgeKind::ConditionalOnFailure => "runs if this fails:",
        EdgeKind::Supersedes => "supersedes",
        EdgeKind::DiscoveredFrom => "discovered from",
    }
}

/// An evidence or artifact line. A pointer naming another item is a
/// reference like any other, so a local-only item stays opaque.
fn pointer(kind: &str, value: &str, ctx: &ProjectionContext) -> String {
    if kind == "item" {
        format!("item: {}", ctx.reference(value))
    } else {
        format!("{kind}: {value}")
    }
}

/// The issue for `view`, or `None` when its kind is never projected.
pub fn project(view: &ItemView<'_>, ctx: &ProjectionContext) -> Option<Projection> {
    let item = view.item;
    let facets = ctx.facets.get(&item.id);
    if !is_projectable(item, facets) {
        return None;
    }
    let mut labels = vec![LABEL_MANAGED.to_string(), kind_label(item)];
    let mut lines: Vec<String> = vec![format!("<!-- rustykrab-item: {} -->", item.id)];

    let status = view.rollup.unwrap_or(item.status);
    let mut head = format!(
        "**Kind:** {} · **Status:** {}",
        item.kind.as_str(),
        phrase(status)
    );
    if view.rollup.is_some() {
        head.push_str(" (roll-up)");
    }
    if let Some(worker) = view.worker {
        head.push_str(&format!(" · **Worker:** {worker}"));
    }
    lines.push(head);
    if item.kind == WorkKind::Proposal {
        let tier = facets.and_then(|f| f.review_tier).unwrap_or_default();
        let subject = facets
            .and_then(|f| f.subject.clone())
            .unwrap_or_else(|| "unspecified".to_string());
        lines.push(format!(
            "**Subject:** {subject} · **Review tier:** {}",
            tier.as_str()
        ));
        labels.push(format!("rustykrab-tier-{}", tier.as_str()));
    }
    if let Some(mode) = facets.and_then(|f| f.capability) {
        lines.push(format!("**Capability:** {}", mode.as_str()));
    }

    lines.push(String::new());
    lines.push("### Objective".to_string());
    lines.push(item.objective.trim().to_string());
    lines.push(String::new());
    lines.push("### Done when".to_string());
    lines.push(item.done_when.trim().to_string());

    if !item.constraints.is_empty() {
        lines.push(String::new());
        lines.push("### Constraints".to_string());
        lines.extend(item.constraints.iter().map(|c| format!("- {}", c.trim())));
    }

    let mut evidence: Vec<String> = view
        .evidence
        .iter()
        .map(|e| pointer(&e.kind, &e.reference, ctx))
        .collect();
    evidence.extend(
        item.artifact_refs
            .iter()
            .map(|r| pointer(&r.kind, &r.value, ctx)),
    );
    evidence.dedup();
    if !evidence.is_empty() {
        lines.push(String::new());
        lines.push("### Evidence".to_string());
        lines.extend(evidence.into_iter().map(|e| format!("- {e}")));
    }

    let mut links: Vec<String> = Vec::new();
    if let Some(parent) = &item.parent {
        links.push(format!("parent: {}", ctx.reference(parent)));
    }
    for e in view.edges {
        links.push(format!(
            "{} {}",
            edge_words(e.kind),
            ctx.reference(&e.depends_on)
        ));
    }
    for input in &item.inputs_from {
        links.push(format!("inputs from {}", ctx.reference(input)));
    }
    if !links.is_empty() {
        lines.push(String::new());
        lines.push("### Links".to_string());
        lines.extend(links.into_iter().map(|l| format!("- {l}")));
    }

    lines.push(String::new());
    lines.push("---".to_string());
    if item.kind == WorkKind::Proposal {
        lines.push(format!(
            "Decide with the label `{LABEL_ACCEPTED}` or `{LABEL_DECLINED}`, or a comment \
             starting `/accept`, `/decline <reason>` or `/amend <text>`."
        ));
    }
    lines.push(
        "Projected from RustyKrab's work store, which is the source of truth: a hand edit to \
         this title or text is overwritten by the next projection."
            .to_string(),
    );

    Some(Projection {
        title: item.title.trim().to_string(),
        body: lines.join("\n"),
        labels,
        open: !item.status.is_closed(),
    })
}

/// A stable digest of the projected fields, so a pass rewrites only what
/// changed.
pub fn digest(p: &Projection) -> String {
    let mut labels = p.labels.clone();
    labels.sort();
    let mut hasher = Sha256::new();
    for part in [
        p.title.as_str(),
        p.body.as_str(),
        &labels.join(","),
        if p.open { "open" } else { "closed" },
    ] {
        hasher.update(part.as_bytes());
        hasher.update(b"\x1f");
    }
    hex::encode(hasher.finalize())[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use rustykrab_core::work::{ArtifactRef, Budget, ReviewTier, Trigger, WorkerKind};

    pub(crate) fn item(id: &str, kind: WorkKind, title: &str) -> WorkItem {
        let now = Utc::now();
        WorkItem {
            id: id.into(),
            kind,
            title: title.into(),
            objective: format!("objective of {title}"),
            done_when: format!("{title} is done"),
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
            status: Status::Queued,
            status_origin: None,
            plan_id: None,
            held_by: None,
            created_at: now,
            updated_at: now,
            closed_at: None,
        }
    }

    fn view(item: &WorkItem) -> ItemView<'_> {
        ItemView {
            item,
            edges: &[],
            evidence: &[],
            worker: None,
            rollup: None,
        }
    }

    fn capability(mode: Option<CapabilityMode>) -> WorkFacets {
        WorkFacets {
            capability: mode,
            ..WorkFacets::default()
        }
    }

    #[test]
    fn the_rule_projects_engineering_and_never_personal_work() {
        let ctx = ProjectionContext::default();
        for kind in [WorkKind::Code, WorkKind::Proposal, WorkKind::Internal] {
            assert!(
                project(&view(&item("a", kind, "t")), &ctx).is_some(),
                "{kind:?}"
            );
        }
        for kind in [WorkKind::Personal, WorkKind::Research] {
            assert!(
                project(&view(&item("a", kind, "t")), &ctx).is_none(),
                "{kind:?}"
            );
        }
        let cap = item("c", WorkKind::Capability, "cap");
        assert!(is_projectable(
            &cap,
            Some(&capability(Some(CapabilityMode::Build)))
        ));
        assert!(!is_projectable(
            &cap,
            Some(&capability(Some(CapabilityMode::Acquire)))
        ));
        assert!(!is_projectable(
            &cap,
            Some(&capability(Some(CapabilityMode::Request)))
        ));
        assert!(
            !is_projectable(&cap, None),
            "an unmarked capability stays local"
        );
    }

    #[test]
    fn a_local_only_neighbour_is_an_opaque_reference() {
        let dentist = item("d1234567-aaaa", WorkKind::Personal, "Book the dentist");
        let fix = item("f7654321-bbbb", WorkKind::Code, "Fix the parser");
        let mut p = item("p0000000-cccc", WorkKind::Proposal, "Pre-load caldav");
        p.inputs_from = vec![dentist.id.clone()];
        p.artifact_refs = vec![ArtifactRef {
            kind: "item".into(),
            value: dentist.id.clone(),
        }];
        let edges = vec![
            Edge {
                item: p.id.clone(),
                depends_on: dentist.id.clone(),
                kind: EdgeKind::WaitsFor,
            },
            Edge {
                item: p.id.clone(),
                depends_on: fix.id.clone(),
                kind: EdgeKind::DiscoveredFrom,
            },
        ];
        let mut ctx = ProjectionContext::default();
        for i in [&dentist, &fix, &p] {
            ctx.items.insert(i.id.clone(), i.clone());
        }
        ctx.issues.insert(fix.id.clone(), "12".into());
        // A personal item is never in `issues`; even a stray row is ignored.
        ctx.issues.insert(dentist.id.clone(), "13".into());
        let v = ItemView {
            item: &p,
            edges: &edges,
            evidence: &[],
            worker: None,
            rollup: None,
        };
        let out = project(&v, &ctx).unwrap();
        assert!(!out.body.contains("Book the dentist"), "{}", out.body);
        assert!(!out.body.contains("objective of Book"), "{}", out.body);
        assert!(
            out.body.contains("waits for local:#d1234567"),
            "{}",
            out.body
        );
        assert!(out.body.contains("inputs from local:#d1234567"));
        assert!(out.body.contains("item: local:#d1234567"));
        assert!(out.body.contains("discovered from #12"));
        assert!(!out.body.contains("#13"));
        assert_eq!(out.title, "Pre-load caldav");
        assert!(out.labels.contains(&"rustykrab-proposal".to_string()));
        assert!(out.labels.contains(&LABEL_MANAGED.to_string()));
    }

    #[test]
    fn the_projection_carries_status_worker_tier_and_closes_with_the_item() {
        let mut p = item("p1", WorkKind::Proposal, "Tune it");
        let mut ctx = ProjectionContext::default();
        ctx.facets.insert(
            "p1".into(),
            WorkFacets {
                capability: None,
                subject: Some("controller".into()),
                review_tier: Some(ReviewTier::Highest),
            },
        );
        let open = project(&view(&p), &ctx).unwrap();
        assert!(open.open);
        assert!(open.body.contains("**Subject:** controller"));
        assert!(open.labels.contains(&"rustykrab-tier-highest".to_string()));
        p.status = Status::Done;
        let closed = project(
            &ItemView {
                worker: Some("pinch"),
                ..view(&p)
            },
            &ctx,
        )
        .unwrap();
        assert!(!closed.open);
        assert!(closed.body.contains("**Worker:** pinch"));
        assert_ne!(digest(&open), digest(&closed));
        assert_eq!(digest(&closed), digest(&closed.clone()));
    }
}
