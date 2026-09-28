//! One pass over the review surface: decisions in, projections out.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use rustykrab_core::proposal::{ProjectionReport, ReviewDecision, ReviewOutcome};
use rustykrab_core::work::{EventKind, WorkFacets, WorkItem, WorkItemId, WorkKind};
use rustykrab_core::Error;
use rustykrab_store::{ProjectionRow, Store, WorkFilter};

use super::project::{digest, is_projectable, project, ItemView, ProjectionContext};
use super::{Comment, DecisionVocabulary, Issue, Projection, ReviewSurface};
use super::{LABEL_ACCEPTED, LABEL_DECLINED};
use crate::handle::ControlHandle;

/// What one pass did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Synced {
    pub report: ProjectionReport,
    /// The decisions applied, in order.
    pub decisions: Vec<ReviewOutcome>,
}

/// The decisions an issue carries, in the order to apply them, each with
/// its actor: trusted comments first, oldest first (amendments, then the
/// first accept or decline), then the decision labels when no comment
/// decided. Both decision labels at once decide nothing.
pub fn parse_decisions(
    surface: &str,
    labels: &[String],
    comments: &[Comment],
) -> Vec<(ReviewDecision, String)> {
    let mut out = Vec::new();
    let mut decided = false;
    for c in comments.iter().filter(|c| c.trusted) {
        let line = c.body.lines().next().unwrap_or_default().trim();
        let actor = format!("reviewer:{surface}:{}", c.author);
        let rest = |cmd: &str| -> Option<String> {
            let tail = line.strip_prefix(cmd)?;
            (tail.is_empty() || tail.starts_with(char::is_whitespace))
                .then(|| tail.trim().to_string())
        };
        if let Some(text) = rest(DecisionVocabulary::AMEND) {
            if !text.is_empty() && !decided {
                out.push((ReviewDecision::Amend { text }, actor));
            }
        } else if rest(DecisionVocabulary::ACCEPT).is_some() {
            if !decided {
                out.push((ReviewDecision::Accept, actor));
                decided = true;
            }
        } else if let Some(reason) = rest(DecisionVocabulary::DECLINE) {
            if !decided {
                out.push((
                    ReviewDecision::Decline {
                        reason: (!reason.is_empty()).then_some(reason),
                    },
                    actor,
                ));
                decided = true;
            }
        }
    }
    if !decided {
        let has = |name: &str| labels.iter().any(|l| l.eq_ignore_ascii_case(name));
        let actor = format!("reviewer:{surface}:label");
        match (has(LABEL_ACCEPTED), has(LABEL_DECLINED)) {
            (true, false) => out.push((ReviewDecision::Accept, actor)),
            (false, true) => out.push((ReviewDecision::Decline { reason: None }, actor)),
            _ => {}
        }
    }
    out
}

/// Comments newer than `after`: GitHub's ids increase with time, so a
/// numeric comparison when both parse, else everything after `after` in
/// the list.
fn newer<'a>(comments: &'a [Comment], after: Option<&str>) -> &'a [Comment] {
    let Some(after) = after else {
        return comments;
    };
    if let Ok(mark) = after.parse::<u64>() {
        let start = comments
            .iter()
            .position(|c| c.id.parse::<u64>().is_ok_and(|id| id > mark))
            .unwrap_or(comments.len());
        return &comments[start..];
    }
    match comments.iter().position(|c| c.id == after) {
        Some(i) => &comments[i + 1..],
        None => comments,
    }
}

/// Whether the issue still shows exactly what the projection wrote.
fn matches(issue: &Issue, p: &Projection) -> bool {
    issue.title == p.title
        && issue.body.trim_end() == p.body.trim_end()
        && issue.open == p.open
        && p.labels
            .iter()
            .all(|l| issue.labels.iter().any(|have| have.eq_ignore_ascii_case(l)))
}

async fn live_items(store: &Store) -> Result<HashMap<WorkItemId, WorkItem>, Error> {
    Ok(store
        .work_list(&WorkFilter {
            include_closed: true,
            ..WorkFilter::default()
        })
        .await?
        .into_iter()
        .map(|i| (i.id.clone(), i))
        .collect())
}

async fn listed(surface: &dyn ReviewSurface) -> Result<HashMap<String, Issue>, Error> {
    Ok(surface
        .issues()
        .await?
        .into_iter()
        .map(|i| (i.number.clone(), i))
        .collect())
}

/// The issue a row names: from the listing, else read on its own (one a
/// human took the managed label off). `Ok(None)` when it is gone.
async fn issue_of(
    surface: &dyn ReviewSurface,
    listed: &HashMap<String, Issue>,
    number: &str,
) -> Result<Option<Issue>, Error> {
    match listed.get(number) {
        Some(issue) => Ok(Some(issue.clone())),
        None => surface.issue(number).await,
    }
}

/// Decisions in: every open projected proposal's decision labels and new
/// trusted comments, applied through `control` as typed review events,
/// oldest first, until one closes the proposal. Only an open proposal
/// takes a decision, so a label left on a closed one is inert.
pub async fn pull_decisions(
    store: &Store,
    surface: &dyn ReviewSurface,
    control: &dyn ControlHandle,
) -> Result<Synced, Error> {
    let name = surface.name().to_string();
    let mut out = Synced::default();
    let rows = store.work_projections_all(&name).await?;
    let items = live_items(store).await?;
    let mut open: Vec<(WorkItemId, ProjectionRow)> = rows
        .into_iter()
        .filter(|(id, _)| {
            items
                .get(id)
                .is_some_and(|i| i.kind == WorkKind::Proposal && !i.status.is_closed())
        })
        .collect();
    if open.is_empty() {
        return Ok(out);
    }
    open.sort_by(|a, b| a.0.cmp(&b.0));
    let listed = listed(surface).await?;
    for (id, mut row) in open {
        let issue = match issue_of(surface, &listed, &row.external_id).await {
            Ok(Some(issue)) => issue,
            Ok(None) => continue,
            Err(e) => {
                out.report
                    .errors
                    .push(format!("read issue {}: {e}", row.external_id));
                continue;
            }
        };
        let comments = match surface.comments(&row.external_id).await {
            Ok(c) => c,
            Err(e) => {
                out.report
                    .errors
                    .push(format!("read comments of {}: {e}", row.external_id));
                continue;
            }
        };
        let fresh = newer(&comments, row.last_comment.as_deref());
        for (decision, actor) in parse_decisions(&name, &issue.labels, fresh) {
            match control.review_decision(&id, decision, &actor).await {
                Ok(outcome) => {
                    out.report.decisions += 1;
                    let closed = outcome.status.is_closed();
                    out.decisions.push(outcome);
                    if closed {
                        break;
                    }
                }
                Err(e) => {
                    out.report.errors.push(format!("decision on {id}: {e}"));
                    break;
                }
            }
        }
        if let Some(last) = comments.last() {
            if row.last_comment.as_deref() != Some(last.id.as_str()) {
                row.last_comment = Some(last.id.clone());
                store.work_projection_put(&row).await?;
            }
        }
    }
    Ok(out)
}

/// Projections out: every projectable live item is written to `surface`,
/// its issue created, or rewritten when the item changed or the issue no
/// longer shows its projection (a hand edit). Per-item failures are
/// reported and the pass goes on; the next pass retries them.
pub async fn push_projections(
    store: &Store,
    surface: &dyn ReviewSurface,
    control: &dyn ControlHandle,
    now: DateTime<Utc>,
) -> Result<ProjectionReport, Error> {
    let name = surface.name().to_string();
    let mut report = ProjectionReport::default();
    let mut rows = store.work_projections_all(&name).await?;
    let items = live_items(store).await?;
    let facets: HashMap<WorkItemId, WorkFacets> = store.work_facets_all().await?;
    let parents: HashSet<WorkItemId> = items.values().filter_map(|i| i.parent.clone()).collect();
    let mut ctx = ProjectionContext {
        items: items.clone(),
        facets,
        issues: rows
            .iter()
            .map(|(id, r)| (id.clone(), r.external_id.clone()))
            .collect(),
    };
    let mut order: Vec<&WorkItem> = items
        .values()
        .filter(|i| is_projectable(i, ctx.facets.get(&i.id)))
        .collect();
    if order.is_empty() {
        return Ok(report);
    }
    order.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
    let listed = listed(surface).await?;
    for item in order {
        let edges = store.work_edges_of(&item.id).await?;
        let evidence = store.work_evidence_list(&item.id).await?;
        let events = store.work_events(&item.id).await?;
        let worker = events
            .iter()
            .rev()
            .find(|e| e.kind == EventKind::Lease)
            .map(|e| {
                e.actor
                    .strip_prefix("worker:")
                    .unwrap_or(&e.actor)
                    .to_string()
            });
        let rollup = if parents.contains(&item.id) {
            control.graph(&item.id).await.ok().and_then(|g| {
                g.nodes
                    .into_iter()
                    .find(|n| n.item.id == item.id)
                    .and_then(|n| n.rollup)
            })
        } else {
            None
        };
        let view = ItemView {
            item,
            edges: &edges,
            evidence: &evidence,
            worker: worker.as_deref(),
            rollup,
        };
        let Some(projection) = project(&view, &ctx) else {
            continue;
        };
        let wanted = digest(&projection);
        let existing = match rows.get(&item.id) {
            Some(row) => match issue_of(surface, &listed, &row.external_id).await {
                Ok(issue) => issue.map(|i| (row.clone(), i)),
                Err(e) => {
                    report
                        .errors
                        .push(format!("read issue {}: {e}", row.external_id));
                    continue;
                }
            },
            None => None,
        };
        let written = match existing {
            Some((row, issue)) => {
                if row.digest == wanted && matches(&issue, &projection) {
                    report.unchanged += 1;
                    continue;
                }
                match surface.update(&row.external_id, &projection).await {
                    Ok(issue) => {
                        report.updated.push(item.id.clone());
                        ProjectionRow {
                            digest: wanted,
                            projected_at: now,
                            url: issue.url.or(row.url),
                            ..row
                        }
                    }
                    Err(e) => {
                        report.errors.push(format!("update {}: {e}", item.id));
                        continue;
                    }
                }
            }
            None => match surface.create(&projection).await {
                Ok(issue) => {
                    report.created.push(item.id.clone());
                    ProjectionRow {
                        item: item.id.clone(),
                        surface: name.clone(),
                        external_id: issue.number.clone(),
                        url: issue.url,
                        digest: wanted,
                        projected_at: now,
                        last_comment: rows.get(&item.id).and_then(|r| r.last_comment.clone()),
                    }
                }
                Err(e) => {
                    report.errors.push(format!("create {}: {e}", item.id));
                    continue;
                }
            },
        };
        ctx.issues
            .insert(item.id.clone(), written.external_id.clone());
        store.work_projection_put(&written).await?;
        rows.insert(item.id.clone(), written);
    }
    Ok(report)
}

/// One whole pass: decisions in, then projections out, so an acceptance
/// read now has its `code` item projected in the same pass.
pub async fn sync(
    store: &Store,
    surface: &dyn ReviewSurface,
    control: &dyn ControlHandle,
    now: DateTime<Utc>,
) -> Result<Synced, Error> {
    let mut out = pull_decisions(store, surface, control).await?;
    let pushed = push_projections(store, surface, control, now).await?;
    out.report.created = pushed.created;
    out.report.updated = pushed.updated;
    out.report.unchanged = pushed.unchanged;
    out.report.errors.extend(pushed.errors);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(id: &str, author: &str, trusted: bool, body: &str) -> Comment {
        Comment {
            id: id.into(),
            author: author.into(),
            trusted,
            body: body.into(),
        }
    }

    #[test]
    fn decisions_come_from_trusted_comments_then_labels() {
        let comments = vec![
            comment("1", "mallory", false, "/accept"),
            comment("2", "ada", true, "/amend only for calendar errands"),
            comment("3", "ada", true, "/decline too broad\nmore text"),
            comment("4", "ada", true, "/accept"),
        ];
        let got = parse_decisions("github", &[LABEL_ACCEPTED.to_string()], &comments);
        assert_eq!(
            got,
            vec![
                (
                    ReviewDecision::Amend {
                        text: "only for calendar errands".into()
                    },
                    "reviewer:github:ada".to_string()
                ),
                (
                    ReviewDecision::Decline {
                        reason: Some("too broad".into())
                    },
                    "reviewer:github:ada".to_string()
                ),
            ]
        );
        assert_eq!(
            parse_decisions("github", &["rustykrab-accepted".into()], &[]),
            vec![(ReviewDecision::Accept, "reviewer:github:label".to_string())]
        );
        assert!(parse_decisions(
            "github",
            &[LABEL_ACCEPTED.into(), LABEL_DECLINED.into()],
            &[]
        )
        .is_empty());
        // `/acceptable` is not `/accept`.
        assert!(
            parse_decisions("github", &[], &[comment("5", "ada", true, "/acceptable")]).is_empty()
        );
    }

    #[test]
    fn only_comments_after_the_mark_are_read() {
        let all = vec![
            comment("9", "a", true, "x"),
            comment("10", "a", true, "y"),
            comment("11", "a", true, "z"),
        ];
        assert_eq!(newer(&all, Some("9")).len(), 2);
        assert_eq!(newer(&all, Some("11")).len(), 0);
        assert_eq!(newer(&all, None).len(), 3);
    }
}
