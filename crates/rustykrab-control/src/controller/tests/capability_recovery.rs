//! Recovery of typed tool references and legacy activation history.
use super::*;
use crate::errors::GapKind;
use crate::routing::CapabilityRef;
use rustykrab_tools::work_backend::ToolState;

#[test]
fn legacy_capability_refs_select_the_real_registered_operation() {
    for name in [
        "work_resources",
        "capability:tool:work_resources",
        "capability:tool:capability:tool:work_resources",
    ] {
        let need = CapabilityRef::parse(&ArtifactRef {
            kind: "capability".into(),
            value: format!("tool:{name}"),
        })
        .unwrap();
        let action = crate::capability_proof::registered_action(&need)
            .expect("a serialized reference must not bypass the functional proof gate");
        assert_eq!(action.operation(), "tool.callable/work_resources");
        assert_eq!(need.to_ref().value, "tool:work_resources");
    }
    // Other namespaces and ordinary colon-containing tool names are not aliases.
    for name in [
        "tool:custom",
        "mcp:server:custom",
        "capability:credential:mail",
        "capability:tool:",
    ] {
        let need = CapabilityRef::parse(&ArtifactRef {
            kind: "capability".into(),
            value: format!("tool:{name}"),
        })
        .unwrap();
        assert_eq!(need.subject, name);
        assert!(crate::capability_proof::registered_action(&need).is_none());
    }
    let credential = CapabilityRef::parse(&ArtifactRef {
        kind: "capability".into(),
        value: "credential:capability:tool:work_resources".into(),
    })
    .unwrap();
    assert_eq!(credential.gap, GapKind::Credential);
    assert_eq!(credential.subject, "capability:tool:work_resources");
}

#[tokio::test]
async fn historical_activation_matches_the_same_requirements_as_the_brief() {
    let h = Harness::new(&["pinch"]);
    // Both names exist in the host catalog, as in the live failure (a late
    // SKILL.md took the serialized reference as its name).
    let mut catalog = StaticCatalog::default();
    for name in ["work_resources", "capability:tool:work_resources"] {
        catalog
            .tools
            .insert(name.into(), ToolState::RegisteredUnloaded);
    }
    let ctl = Controller::new(
        h.store.clone(),
        vec![Arc::new(Scripted {
            name: "pinch".into(),
            concurrency: 1,
            script: h.script.clone(),
        })],
        ControllerConfig::default(),
    )
    .with_catalog(Arc::new(catalog));
    let hist = ctl.activatable(super::super::load::History {
        activate: vec!["capability:tool:work_resources".into()],
        ..Default::default()
    });
    assert_eq!(hist.activate, vec!["work_resources"]);
    let id = h.file_one(draft("w", "Read inventory")).await;
    let item = h.item(&id).await;
    assert!(
        ctl.pick(&item, &hist, &HashMap::new(), &Default::default(), &[])
            .is_none(),
        "a worker missing the activated real tool must not receive the lease"
    );
    assert!(h.script.briefs().is_empty());
}

struct CeilingWorker {
    run: Scripted,
    inventory: bool,
    resources: Vec<String>,
}
#[async_trait]
impl Worker for CeilingWorker {
    fn name(&self) -> &str {
        self.run.name()
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::Local
    }
    fn capabilities(&self) -> WorkerCapabilities {
        let mut caps = self.run.capabilities();
        if self.inventory {
            caps.tools.push("work_resources".into());
        }
        caps.writable_resources = self.resources.clone();
        caps
    }
    async fn run(&self, brief: Brief) -> Result<ResultReport, Error> {
        self.run.run(brief).await
    }
}

#[tokio::test]
async fn a_local_tool_ceiling_gap_routes_once_or_surfaces_without_an_acquisition_chain() {
    use rustykrab_core::work::{BlockedReason, BlockedReport, WorkKind};
    for alternative_scope in [Some("repo:/fixture"), Some("repo:/other"), None] {
        let script = Arc::new(Script::default());
        let make = |name: &str, inventory, resource: &str| {
            Arc::new(CeilingWorker {
                run: Scripted {
                    name: name.into(),
                    concurrency: 1,
                    script: script.clone(),
                },
                inventory,
                resources: vec![resource.into()],
            }) as Arc<dyn Worker>
        };
        let mut fleet = vec![make("narrow", false, "repo:/fixture")];
        if let Some(scope) = alternative_scope {
            fleet.push(make("inventory", true, scope));
        }
        let mut catalog = StaticCatalog::default();
        catalog
            .tools
            .insert("work_resources".into(), ToolState::Loaded);
        let h = super::proof::harness(ControllerConfig::default(), catalog, script.clone(), fleet);
        script.push(
            "Read inventory",
            report(ResultReport {
                summary: "Need inventory".into(),
                blocked: Some(BlockedReport {
                    reason: BlockedReason::NeedsTool,
                    detail: "inventory unavailable".into(),
                    needs: vec!["capability:tool:work_resources".into()],
                }),
                ..Default::default()
            }),
        );
        let mut d = draft("w", "Read inventory");
        d.writable_resources = vec!["repo:/fixture".into()];
        let id = h.file_one(d).await;
        h.drain().await;
        assert!(
            h.of_kind(WorkKind::Capability).await.is_empty(),
            "a ceiling change cannot be acquired by a model"
        );
        let runs = script.briefs_for("Read inventory");
        if alternative_scope == Some("repo:/fixture") {
            assert_eq!(h.status(&id).await, Status::Done);
            assert_eq!(runs.len(), 2);
            assert_eq!(runs[1].required_tools, vec!["work_resources"]);
            assert_eq!(runs[1].writable_resources, vec!["repo:/fixture"]);
        } else {
            assert_eq!(
                h.status(&id).await,
                Status::Blocked(BlockedReason::NeedsDecision)
            );
            assert_eq!(runs.len(), 1);
            let questions = h
                .store
                .questions_list(&rustykrab_store::QuestionFilter {
                    item: Some(id.clone()),
                    ..Default::default()
                })
                .await
                .unwrap();
            assert_eq!(questions.len(), 1);
            let before = h.events(&id).await;
            for _ in 0..5 {
                h.step().await;
            }
            assert_eq!(
                h.events(&id).await,
                before,
                "an unchanged ceiling hold must stay quiet"
            );
        }
        assert!(h
            .store
            .work_evidence_list(&id)
            .await
            .unwrap()
            .iter()
            .all(|e| e.kind != "budget_grant"));
    }
}

#[tokio::test]
async fn a_legacy_done_claim_cannot_bypass_proof_or_release_an_independent_owner_hold() {
    use rustykrab_core::work::{BlockedReason, WorkKind};
    let script = Arc::new(Script::default());
    let prober = Arc::new(super::proof::Prober::new("planner", &script));
    let h = super::proof::harness(
        ControllerConfig::default(),
        StaticCatalog::default(),
        script,
        vec![prober],
    );
    let id = h.file_one(draft("w", "Original inventory operation")).await;
    let cap_id = h.file_one(draft("c", "Legacy acquisition")).await;
    let mut waiting = h.item(&id).await;
    let id = Uuid::new_v4().to_string();
    waiting.id = id.clone();
    waiting.status = Status::Blocked(BlockedReason::NeedsTool);
    waiting.held_by = Some("independent-owner-hold".into());
    let before = waiting.clone();
    let mut cap = h.item(&cap_id).await;
    let cap_id = Uuid::new_v4().to_string();
    cap.id = cap_id.clone();
    cap.kind = WorkKind::Capability;
    cap.status = Status::Done;
    cap.artifact_refs = vec![ArtifactRef {
        kind: "capability".into(),
        value: "tool:capability:tool:work_resources".into(),
    }];
    let edges = vec![rustykrab_core::work::Edge {
        item: id.clone(),
        kind: EdgeKind::Blocks,
        depends_on: cap_id.clone(),
    }];
    h.store
        .work_insert_graph(&[waiting.clone(), cap.clone()], &edges, None)
        .await
        .unwrap();
    h.store
        .work_event_append(&WorkEvent {
            item: id.clone(),
            kind: EventKind::Lease,
            actor: "worker:planner".into(),
            from: None,
            to: None,
            reason: None,
            evidence_ref: None,
            origin: None,
            upstream: None,
            at: h.clock.now(),
        })
        .await
        .unwrap();
    let snap = crate::graph::Snapshot::new(vec![waiting, cap], edges);
    // The prober has no scripted observation. A nested legacy ref must now
    // require ToolCallable proof, rather than count as availability-only.
    assert!(!h.ctl.revival_proven(&snap, &id).await.unwrap());
    let generations = h.store.work_evidence_list(&id).await.unwrap();
    assert_eq!(
        generations
            .iter()
            .filter(|e| e.kind == "repair_generation")
            .count(),
        1
    );
    assert!(generations.iter().all(|e| e.kind != "budget_grant"));
    assert_eq!(
        h.item(&id).await,
        before,
        "durable independent owner hold remains intact"
    );
    let receipts = h.store.work_evidence_list(&cap_id).await.unwrap();
    let receipt = receipts
        .iter()
        .find(|e| e.kind == "operation_probe")
        .unwrap();
    let probe: crate::capability_proof::OperationProbe =
        serde_json::from_str(&receipt.reference).unwrap();
    assert_eq!(probe.operation, "tool.callable/work_resources");
    assert_eq!(
        probe.outcome,
        crate::capability_proof::ProbeOutcome::Unobserved
    );
    assert!(!h.ctl.revival_proven(&snap, &id).await.unwrap());
    assert_eq!(
        h.store.work_evidence_list(&id).await.unwrap(),
        generations,
        "unchanged failure does not mint another generation or spend"
    );
    assert_eq!(h.item(&id).await, before);
    assert!(h.script.briefs().is_empty());
}
