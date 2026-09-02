//! Black-box acceptance scenarios for conversational project planning.
//!
//! The fixture/replay scenario is real infrastructure and must pass today.
//! Project behaviors are deliberately `XFail` until their construction slices
//! land.  Because the common report classifies an unexpected pass as `xpass`,
//! implementing a behavior forces its scenario to be reviewed and promoted.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

use crate::fixture_repo::{FixtureRepo, DELIVERY_PLANNING};
use crate::{Ctx, Expected, ScenarioFn};

/// Stable scenario catalog. IDs are an external interface for CI filters and
/// construction-stack promotion; rename them only as an intentional migration.
pub(crate) fn scenarios() -> Vec<(Expected, (&'static str, ScenarioFn))> {
    vec![
        (
            Expected::Pass,
            (
                "planning/fixture-repository-and-conversation-replay",
                boxed(fixture_replay),
            ),
        ),
        (
            Expected::XFail,
            (
                "planning/vague-project-begins-without-schema",
                boxed(vague_project),
            ),
        ),
        (
            Expected::XFail,
            (
                "planning/material-question-records-decision",
                boxed(material_question),
            ),
        ),
        (
            Expected::XFail,
            (
                "planning/restart-compaction-rehydrates-linked-state",
                boxed(restart_compaction),
            ),
        ),
        (
            Expected::XFail,
            (
                "planning/decision-correction-preserves-history",
                boxed(decision_correction),
            ),
        ),
        (
            Expected::XFail,
            (
                "planning/progressive-readiness-keeps-future-question-nonblocking",
                boxed(progressive_readiness),
            ),
        ),
        (
            Expected::XFail,
            (
                "planning/projections-share-source-revision",
                boxed(projection_consistency),
            ),
        ),
        (
            Expected::XFail,
            (
                "planning/authorized-slice-freezes-revision-and-authority",
                boxed(frozen_execution),
            ),
        ),
        (
            Expected::XFail,
            (
                "planning/delivery-result-reconciles-roadmap",
                boxed(result_reconciliation),
            ),
        ),
    ]
}

fn boxed(run: ScenarioFn) -> ScenarioFn {
    run
}

macro_rules! scenario_fn {
    ($name:ident, $body:expr) => {
        fn $name(
            ctx: &Ctx,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + '_>> {
            Box::pin(async move { $body(ctx).await })
        }
    };
}

async fn fixture_replay_impl(_ctx: &Ctx) -> Result<()> {
    let repo = FixtureRepo::create()?;
    repo.verify()?;

    let turns: Vec<_> = DELIVERY_PLANNING.replay().collect();
    if turns.len() != 5 || !turns[0].content.starts_with("I want") {
        bail!("planning conversation is not a stable vague-to-action replay");
    }
    let resumed: Vec<_> = DELIVERY_PLANNING
        .replay_after("message-003-decision")?
        .collect();
    if resumed.first().map(|turn| turn.message_id) != Some("message-004-correction") {
        bail!("conversation did not resume immediately after its checkpoint");
    }
    Ok(())
}
scenario_fn!(fixture_replay, fixture_replay_impl);

async fn create_vague_project(ctx: &Ctx) -> Result<(FixtureRepo, Value)> {
    let repo = FixtureRepo::create()?;
    let opening = DELIVERY_PLANNING.opening();
    let response = ctx
        .post(
            "/api/projects",
            json!({
                "repository_path": repo.path(),
                "base_revision": repo.head_sha(),
                "canonical_conversation_id": DELIVERY_PLANNING.conversation_id,
                "source_message_id": opening.message_id,
                "message": opening.content
            }),
        )
        .await?;
    if response.status() != 201 {
        bail!(
            "create vague project returned {}, want 201",
            response.status()
        );
    }
    let project: Value = response.json().await?;
    require_string(&project, "id")?;
    require_string(&project, "current_revision")?;
    if project["revision_count"] != 1 {
        bail!("new project must expose revision_count=1: {project}");
    }
    Ok((repo, project))
}

async fn vague_project_impl(ctx: &Ctx) -> Result<()> {
    let (_repo, project) = create_vague_project(ctx).await?;
    let project_id = require_string(&project, "id")?;
    let brief = get_json(
        ctx,
        &format!("/api/projects/{project_id}/projections/brief"),
    )
    .await?;
    if brief["source_revision"] != project["current_revision"] {
        bail!("brief is not sourced from the created revision: {brief}");
    }
    if brief["intent"].as_str().is_none() {
        bail!("ordinary-language opening did not produce a project intent: {brief}");
    }

    Ok(())
}
scenario_fn!(vague_project, vague_project_impl);

async fn material_question_impl(ctx: &Ctx) -> Result<()> {
    let (_repo, project) = create_vague_project(ctx).await?;
    let project_id = require_string(&project, "id")?;
    let parent_revision = require_string(&project, "current_revision")?;
    let turns: Vec<_> = DELIVERY_PLANNING.replay().collect();
    let response = ctx
        .post(
            &format!("/api/projects/{project_id}/planning/replay"),
            json!({
                "parent_revision": parent_revision,
                "turns": turns[1..3].iter().map(|turn| json!({
                    "conversation_id": DELIVERY_PLANNING.conversation_id,
                    "message_id": turn.message_id,
                    "role": turn.role,
                    "content": turn.content
                })).collect::<Vec<_>>()
            }),
        )
        .await?;
    let state = expect_json(response, 200, "replay material question").await?;
    if state["questions"][0]["status"] != "resolved" {
        bail!("material question was not recorded and decided: {state}");
    }
    let decision = &state["decisions"][0];
    if decision["source_message_id"] != "message-003-decision"
        || decision["selected_option"].as_str().is_none()
    {
        bail!("answer did not become a provenance-linked decision: {decision}");
    }
    Ok(())
}
scenario_fn!(material_question, material_question_impl);

async fn restart_compaction_impl(ctx: &Ctx) -> Result<()> {
    let (_repo, project) = create_vague_project(ctx).await?;
    let project_id = require_string(&project, "id")?;
    let snapshot = get_json(ctx, &format!("/api/projects/{project_id}/snapshot")).await?;
    if snapshot["current_revision"] != project["current_revision"]
        || snapshot["revision_count"] != project["revision_count"]
    {
        bail!("rehydrated snapshot changed revision identity: {snapshot}");
    }
    if snapshot.get("transcript").is_some() || snapshot.get("messages").is_some() {
        bail!("project snapshot copied conversation text instead of linking it: {snapshot}");
    }
    let links = snapshot["conversation_links"]
        .as_array()
        .ok_or_else(|| anyhow!("snapshot has no conversation_links: {snapshot}"))?;
    let linked = links.iter().any(|link| {
        link["conversation_id"] == DELIVERY_PLANNING.conversation_id
            && link["source_message_ids"]
                .as_array()
                .is_some_and(|ids| ids.iter().any(|id| id == "message-001-vague-idea"))
    });
    if !linked {
        bail!("snapshot lost its source conversation/message links: {snapshot}");
    }
    Ok(())
}
scenario_fn!(restart_compaction, restart_compaction_impl);

async fn decision_correction_impl(ctx: &Ctx) -> Result<()> {
    let (_repo, project) = create_vague_project(ctx).await?;
    let project_id = require_string(&project, "id")?;
    let parent_revision = require_string(&project, "current_revision")?;
    let turns: Vec<_> = DELIVERY_PLANNING.replay().collect();
    let response = ctx
        .post(
            &format!("/api/projects/{project_id}/planning/replay"),
            json!({
                "parent_revision": parent_revision,
                "turns": turns[1..4].iter().map(|turn| json!({
                    "conversation_id": DELIVERY_PLANNING.conversation_id,
                    "message_id": turn.message_id,
                    "role": turn.role,
                    "content": turn.content
                })).collect::<Vec<_>>()
            }),
        )
        .await?;
    let state = expect_json(response, 200, "replay correction").await?;
    let history = state["decision_history"]
        .as_array()
        .ok_or_else(|| anyhow!("correction has no decision_history: {state}"))?;
    let superseded = history.iter().any(|decision| {
        decision["active"] == false && decision["superseded_by"].as_str().is_some()
    });
    let corrected = history.iter().any(|decision| {
        decision["active"] == true && decision["source_message_id"] == "message-004-correction"
    });
    if !superseded || !corrected {
        bail!("correction erased history or failed to become current: {history:?}");
    }
    Ok(())
}
scenario_fn!(decision_correction, decision_correction_impl);

async fn progressive_readiness_impl(ctx: &Ctx) -> Result<()> {
    let (_repo, project) = create_vague_project(ctx).await?;
    let project_id = require_string(&project, "id")?;
    let response = ctx
        .post(
            &format!("/api/projects/{project_id}/readiness"),
            json!({
                "candidate_slice": "durable planning model",
                "deferred_question": "Which production environment will deploy it?"
            }),
        )
        .await?;
    let readiness = expect_json(response, 200, "assess progressive readiness").await?;
    if readiness["candidate_slice"]["status"] != "proposable"
        || readiness["deferred_questions"][0]["blocking_scope"] != "later"
    {
        bail!("future deployment question blocked a ready local slice: {readiness}");
    }
    Ok(())
}
scenario_fn!(progressive_readiness, progressive_readiness_impl);

async fn projection_consistency_impl(ctx: &Ctx) -> Result<()> {
    let (_repo, project) = create_vague_project(ctx).await?;
    let project_id = require_string(&project, "id")?;
    let projections = get_json(ctx, &format!("/api/projects/{project_id}/projections")).await?;
    let source_revision = require_string(&projections, "source_revision")?;
    for name in ["brief", "roadmap", "decisions", "questions"] {
        if projections[name]["source_revision"] != source_revision {
            bail!("{name} projection disagrees about source revision: {projections}");
        }
    }
    let current_scope = &projections["brief"]["scope"];
    if projections["roadmap"]["scope"] != *current_scope {
        bail!("brief and roadmap disagree about current scope: {projections}");
    }
    Ok(())
}
scenario_fn!(projection_consistency, projection_consistency_impl);

async fn frozen_execution_impl(ctx: &Ctx) -> Result<()> {
    let (_repo, project) = create_vague_project(ctx).await?;
    let project_id = require_string(&project, "id")?;
    let project_revision = require_string(&project, "current_revision")?;
    let response = ctx
        .post(
            &format!("/api/projects/{project_id}/slices"),
            json!({
                "project_revision": project_revision,
                "objective": "Persist the project model",
                "authority": { "repository_write": true, "network": false, "merge": false }
            }),
        )
        .await?;
    let slice = expect_json(response, 201, "propose execution slice").await?;
    let slice_id = require_string(&slice, "id")?;
    let response = ctx
        .post(
            &format!("/api/projects/{project_id}/slices/{slice_id}/authorize"),
            json!({"source_message_id": "message-005-authorization"}),
        )
        .await?;
    let frozen = expect_json(response, 200, "authorize execution slice").await?;
    if frozen["project_revision"] != project_revision
        || frozen["authority_snapshot"] != slice["authority"]
        || frozen["manifest_hash"].as_str().is_none()
    {
        bail!("authorization did not freeze revision, authority, and hash: {frozen}");
    }
    Ok(())
}
scenario_fn!(frozen_execution, frozen_execution_impl);

async fn result_reconciliation_impl(ctx: &Ctx) -> Result<()> {
    let (_repo, project) = create_vague_project(ctx).await?;
    let project_id = require_string(&project, "id")?;
    let before_revision = require_string(&project, "current_revision")?;
    let response = ctx
        .post(
            &format!("/api/projects/{project_id}/delivery-results"),
            json!({
                "delivery_id": "delivery-fixture-001",
                "source_project_revision": before_revision,
                "status": "verified",
                "implemented_outcomes": ["durable planning model"],
                "evidence": [{"kind": "test", "sha": "fixture-head", "passed": true}]
            }),
        )
        .await?;
    let reconciled = expect_json(response, 200, "reconcile delivery result").await?;
    if reconciled["current_revision"] == before_revision
        || reconciled["milestones"][0]["status"] != "complete"
        || reconciled["readiness"]["recomputed"] != true
    {
        bail!("delivery result did not advance plan and readiness: {reconciled}");
    }
    Ok(())
}
scenario_fn!(result_reconciliation, result_reconciliation_impl);

async fn get_json(ctx: &Ctx, path: &str) -> Result<Value> {
    let response = ctx.get(path).await?;
    expect_json(response, 200, path).await
}

async fn expect_json(response: reqwest::Response, status: u16, operation: &str) -> Result<Value> {
    if response.status().as_u16() != status {
        bail!("{operation} returned {}, want {status}", response.status());
    }
    Ok(response.json().await?)
}

fn require_string<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value[field]
        .as_str()
        .ok_or_else(|| anyhow!("{field} missing from response: {value}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_stable_unique_planning_ids() {
        let scenarios = scenarios();
        let mut ids: Vec<_> = scenarios.iter().map(|(_, (id, _))| *id).collect();
        assert!(ids.iter().all(|id| id.starts_with("planning/")));
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count);
    }

    #[test]
    fn only_fixture_infrastructure_is_promoted_initially() {
        let scenarios = scenarios();
        let promoted: Vec<_> = scenarios
            .iter()
            .filter(|(expected, _)| *expected == Expected::Pass)
            .map(|(_, (id, _))| *id)
            .collect();
        assert_eq!(
            promoted,
            vec!["planning/fixture-repository-and-conversation-replay"]
        );
        assert_eq!(
            scenarios
                .iter()
                .filter(|(expected, _)| *expected == Expected::XFail)
                .count(),
            8
        );
    }
}
