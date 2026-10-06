//! What a run is given (plan section 6, step 4, and 6.3): the typed brief
//! and its fan-in block.
//!
//! The inputs block copies, from each item in `inputs_from` and each
//! `capability` item the ladder parked the item behind (section 8: the
//! original resumes with what the build produced), pointers only:
//! its verified evidence refs and artifact refs, its closed status, one
//! line of its result summary and, for a failed input, its error. A parent
//! input contributes its verification record and its done leaves' refs. The
//! block is capped at `GraphCaps::max_inputs` entries and roughly
//! `max_input_tokens`; the rest are listed by id in `more_inputs`. The
//! copied set is stored with the lease, so the brief can be rebuilt.

use rustykrab_core::work::{
    ArtifactRef, EdgeKind, EventKind, Evidence, InputRef, Status, WorkItem, WorkItemId, WorkKind,
};
use rustykrab_core::Error;

use crate::graph::Snapshot;
use crate::worker::Brief;

use super::load::{last_error, History};
use super::Controller;

/// Evidence kinds the controller writes that are not refs to hand on.
pub const SUMMARY: &str = "summary";
pub(super) const ERROR: &str = "error";
/// Who verified an artifact in Phase 1: its presence in the report.
pub(super) const RESULT_REPORT: &str = "result_report";
/// The evidence kind of a run's pointer: the id the worker keeps its
/// transcript under, written at lease time. Unverified, so never handed on
/// as an input.
pub(super) const RUN: &str = "run";

/// Refs one input may carry, each of evidence and artifacts.
const REFS_PER_INPUT: usize = 8;

/// A rough token count for the inputs cap: four characters a token.
fn tokens(input: &InputRef) -> usize {
    serde_json::to_string(input).map_or(0, |s| s.len() / 4)
}

pub(super) fn first_line(text: &str) -> String {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let line = line.trim();
    match line.char_indices().nth(200) {
        Some((cut, _)) => format!("{}...", &line[..cut]),
        None => line.to_string(),
    }
}

fn push_ref(list: &mut Vec<ArtifactRef>, r: ArtifactRef) {
    if !list.contains(&r) && list.len() < REFS_PER_INPUT {
        list.push(r);
    }
}

impl Controller {
    /// The inputs block for `item`, capped, and the ids beyond the caps.
    pub(super) async fn build_inputs(
        &self,
        snap: &Snapshot,
        item: &WorkItem,
    ) -> Result<(Vec<InputRef>, Vec<WorkItemId>), Error> {
        let mut inputs: Vec<InputRef> = Vec::new();
        let mut more: Vec<WorkItemId> = Vec::new();
        let max = self.config.caps.max_inputs as usize;
        let budget = self.config.caps.max_input_tokens as usize;
        let mut spent = 0usize;
        // The capability items the ladder parked this item behind are
        // inputs too, once done: the resumed run starts from what the
        // build or acquisition reported (section 8).
        let capabilities: Vec<WorkItemId> = snap
            .edges_held_by(&item.id)
            .filter(|e| e.kind == EdgeKind::Blocks)
            .filter(|e| !item.inputs_from.contains(&e.depends_on))
            .filter(|e| {
                snap.item(&e.depends_on)
                    .is_some_and(|u| u.kind == WorkKind::Capability && u.status == Status::Done)
            })
            .map(|e| e.depends_on.clone())
            .collect();
        for id in item.inputs_from.iter().chain(&capabilities) {
            let Some(up) = snap.item(id) else {
                more.push(id.clone());
                continue;
            };
            let input = self.input(snap, item, up).await?;
            let cost = tokens(&input);
            let over = inputs.len() >= max || (!inputs.is_empty() && spent + cost > budget);
            if over {
                more.push(id.clone());
                continue;
            }
            spent += cost;
            inputs.push(input);
        }
        Ok((inputs, more))
    }

    async fn input(
        &self,
        snap: &Snapshot,
        item: &WorkItem,
        up: &WorkItem,
    ) -> Result<InputRef, Error> {
        let edge = snap
            .edges_held_by(&item.id)
            .find(|e| e.depends_on == up.id && e.kind.is_ordering())
            .map(|e| e.kind);
        let mut evidence: Vec<ArtifactRef> = Vec::new();
        let mut summary = String::new();
        let is_parent = snap.has_children(&up.id);
        let sources: Vec<WorkItemId> = if is_parent {
            snap.descendants(&up.id)
                .into_iter()
                .filter(|d| !snap.has_children(d) && snap.status(d) == Some(Status::Done))
                .collect()
        } else {
            vec![up.id.clone()]
        };
        for source in &sources {
            for ev in self.store.work_evidence_list(source).await? {
                if ev.kind == SUMMARY {
                    if !is_parent {
                        summary = first_line(&ev.reference);
                    }
                    continue;
                }
                if ev.kind == ERROR || ev.verified_by.is_none() {
                    continue;
                }
                push_ref(
                    &mut evidence,
                    ArtifactRef {
                        kind: ev.kind,
                        value: ev.reference,
                    },
                );
            }
        }
        let mut error = None;
        if is_parent || up.status == Status::Failed {
            let events = self.store.work_events(&up.id).await?;
            if is_parent {
                // The parent's verification record: the reason on its `done`.
                if let Some(e) = events
                    .iter()
                    .rev()
                    .find(|e| e.kind == EventKind::Transition && e.to == Some(Status::Done))
                {
                    summary = e.reason.clone().unwrap_or_default();
                }
            }
            if up.status == Status::Failed {
                error = last_error(&events);
            }
        }
        let mut artifacts = Vec::new();
        for r in &up.artifact_refs {
            push_ref(&mut artifacts, r.clone());
        }
        Ok(InputRef {
            item: up.id.clone(),
            title: up.title.clone(),
            status: up.status,
            edge,
            evidence,
            artifacts,
            summary,
            error,
        })
    }
}

/// The brief for one run (plan section 6, step 4): the item's typed fields,
/// its inputs, and for a repair run its own evidence and last error. Tools
/// a capability rung acquired are activated up front.
pub(super) fn brief_for(
    item: &WorkItem,
    inputs: Vec<InputRef>,
    more_inputs: Vec<WorkItemId>,
    prior_evidence: Vec<Evidence>,
    history: &History,
) -> Brief {
    let mut required_tools = item.required_tools.clone();
    for tool in &history.activate {
        if !required_tools.contains(tool) {
            required_tools.push(tool.clone());
        }
    }
    Brief {
        item: item.id.clone(),
        kind: item.kind,
        title: item.title.clone(),
        objective: item.objective.clone(),
        done_when: item.done_when.clone(),
        constraints: item.constraints.clone(),
        decisions_made: item.decisions_made.clone(),
        artifact_refs: item.artifact_refs.clone(),
        required_tools,
        required_mcp_servers: item.required_mcp_servers.clone(),
        writable_resources: item.writable_resources.clone(),
        inputs,
        more_inputs,
        prior_evidence,
        last_error: history.repair.clone(),
        budget: item.budget,
        origin_conversation_id: item.origin_conversation_id.clone(),
        run: None,
        workspace: None,
        capability: None,
        project_context: None,
    }
}
