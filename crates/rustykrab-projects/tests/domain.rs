use chrono::{DateTime, Utc};
use rustykrab_projects::{
    AssumptionState, AssumptionStatus, CreateProject, DecisionMaker, DecisionState, DecisionStatus,
    EdgeId, EdgeRelation, JudgmentPolicy, MilestoneState, MilestoneStatus, NodeData, NodeId,
    NodeKind, OutcomeState, OutcomeStatus, PlanChange, PlanChangeSet, PlanEdgeDraft, PlanNodeDraft,
    PlanNodePatch, ProjectError, ProjectId, ProjectPatch, ProjectSnapshot, Provenance,
    ProvenanceClassification, ProvenanceSource, QuestionImpact, QuestionState, QuestionStatus,
    RevisionAuthor, RiskLevel, RiskState, RiskStatus,
};
use uuid::Uuid;

fn timestamp(seconds: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(seconds, 0).expect("valid test timestamp")
}

fn project_id(value: u128) -> ProjectId {
    ProjectId::from_uuid(Uuid::from_u128(value))
}

fn node_id(value: u128) -> NodeId {
    NodeId::from_uuid(Uuid::from_u128(value))
}

fn edge_id(value: u128) -> EdgeId {
    EdgeId::from_uuid(Uuid::from_u128(value))
}

fn provenance(reference: &str, at: i64) -> Provenance {
    Provenance {
        classification: ProvenanceClassification::UserStated,
        source: ProvenanceSource::Manual {
            reference: reference.to_owned(),
        },
        recorded_at: timestamp(at),
        confidence: None,
        freshness: None,
    }
}

fn generic_node(id: u128, kind: NodeKind, title: &str, source: &str) -> PlanNodeDraft {
    PlanNodeDraft::new(
        node_id(id),
        kind,
        title,
        format!("Details for {title}"),
        NodeData::generic("active"),
        vec![provenance(source, 1_700_000_000)],
    )
}

fn create_project(initial_changes: Vec<PlanChange>) -> ProjectSnapshot {
    let mut command = CreateProject::new(
        "create-request",
        project_id(1),
        "Autonomous delivery",
        RevisionAuthor::User,
        timestamp(1_700_000_000),
    );
    command.repository_id = Some("github:gcbh/rustykrab".to_owned());
    command.canonical_conversation_id = Some("conversation-1".to_owned());
    command.judgment_policy = JudgmentPolicy {
        statement: "Delegate reversible implementation details".to_owned(),
        delegated_scopes: vec!["internal implementation".to_owned()],
        reserved_decisions: vec!["security boundary".to_owned()],
        ..JudgmentPolicy::default()
    };
    command.initial_changes = initial_changes;
    ProjectSnapshot::create(command).expect("valid project")
}

#[test]
fn identical_inputs_produce_identical_revisions_and_serde() {
    let initial = vec![
        PlanChange::AddNode {
            node: generic_node(10, NodeKind::Intent, "Deliver long-term plans", "message-1"),
        },
        PlanChange::AddNode {
            node: PlanNodeDraft::new(
                node_id(11),
                NodeKind::Outcome,
                "Verified changes",
                "Every layer provides functional proof",
                NodeData::Outcome(OutcomeState {
                    status: OutcomeStatus::Desired,
                    success_measures: vec!["Every layer passes independent checks".to_owned()],
                }),
                vec![provenance("message-1", 1_700_000_000)],
            ),
        },
        PlanChange::AddEdge {
            edge: PlanEdgeDraft::new(
                edge_id(20),
                node_id(10),
                EdgeRelation::RelatedTo,
                node_id(11),
                vec![provenance("message-1", 1_700_000_000)],
            ),
        },
    ];
    let left = create_project(initial.clone());
    let right = create_project(initial);
    assert_eq!(left, right);
    assert_eq!(left.revision.id.as_str().len(), 64);

    let changes = PlanChangeSet::new(
        "change-request",
        left.project.id,
        left.revision.id.clone(),
        "Add first milestone",
        RevisionAuthor::Agent,
        timestamp(1_700_000_100),
        vec![PlanChange::AddNode {
            node: PlanNodeDraft::new(
                node_id(12),
                NodeKind::Milestone,
                "Durable planning",
                "Project state survives restart",
                NodeData::Milestone(MilestoneState {
                    status: MilestoneStatus::Planned,
                    exit_conditions: vec!["Reloaded state is identical".to_owned()],
                }),
                vec![provenance("agent-research-1", 1_700_000_100)],
            ),
        }],
    );
    let left_next = left.apply(changes.clone()).expect("valid revision");
    let right_next = right.apply(changes).expect("valid revision");
    assert_eq!(left_next.revision.id, right_next.revision.id);
    assert_eq!(left_next, right_next);

    let encoded = serde_json::to_string(&left_next).expect("serialize snapshot");
    let decoded: ProjectSnapshot = serde_json::from_str(&encoded).expect("deserialize snapshot");
    assert_eq!(decoded, left_next);
    assert_eq!(
        decoded.revision.projections(),
        left_next.revision.projections()
    );
}

#[test]
fn missing_provenance_rejects_the_entire_change_set() {
    let snapshot = create_project(Vec::new());
    let original = snapshot.clone();
    let changes = PlanChangeSet::new(
        "missing-source",
        snapshot.project.id,
        snapshot.revision.id.clone(),
        "Attempt an untraceable update",
        RevisionAuthor::Agent,
        timestamp(1_700_000_100),
        vec![
            PlanChange::AddNode {
                node: generic_node(30, NodeKind::Requirement, "Traceable", "message-2"),
            },
            PlanChange::AddNode {
                node: PlanNodeDraft::new(
                    node_id(31),
                    NodeKind::Constraint,
                    "Missing source",
                    "This must fail",
                    NodeData::generic("active"),
                    Vec::new(),
                ),
            },
        ],
    );

    assert_eq!(
        snapshot.apply(changes),
        Err(ProjectError::MissingProvenance { change_index: 1 })
    );
    assert_eq!(snapshot, original);
}

#[test]
fn invalid_relationship_rejects_the_transaction() {
    let snapshot = create_project(vec![
        PlanChange::AddNode {
            node: generic_node(40, NodeKind::Requirement, "Store history", "message-3"),
        },
        PlanChange::AddNode {
            node: PlanNodeDraft::new(
                node_id(41),
                NodeKind::Question,
                "Which store?",
                "Choose a persistence mechanism",
                NodeData::Question(QuestionState {
                    status: QuestionStatus::Open,
                    impact: QuestionImpact::BlockingLater,
                    decision_owner: rustykrab_projects::DecisionOwner::User,
                    blocking_scope: None,
                    default_action: None,
                    due_milestone: None,
                    resolution: None,
                }),
                vec![provenance("message-3", 1_700_000_000)],
            ),
        },
    ]);
    let invalid_edge = edge_id(42);
    let changes = PlanChangeSet::new(
        "invalid-edge",
        snapshot.project.id,
        snapshot.revision.id.clone(),
        "Add an invalid resolution edge",
        RevisionAuthor::Agent,
        timestamp(1_700_000_100),
        vec![PlanChange::AddEdge {
            edge: PlanEdgeDraft::new(
                invalid_edge,
                node_id(40),
                EdgeRelation::Resolves,
                node_id(41),
                vec![provenance("agent-1", 1_700_000_100)],
            ),
        }],
    );

    assert!(matches!(
        snapshot.apply(changes),
        Err(ProjectError::InvalidRelationship { edge_id, .. }) if edge_id == invalid_edge
    ));
    assert!(snapshot.revision.edges.is_empty());
}

#[test]
fn supersession_preserves_history_and_retires_stale_relationships() {
    let old_decision = node_id(50);
    let replacement = node_id(51);
    let question = node_id(52);
    let link = edge_id(53);
    let snapshot = create_project(vec![
        PlanChange::AddNode {
            node: PlanNodeDraft::new(
                old_decision,
                NodeKind::Decision,
                "Use YAML as the interface",
                "Initial proposal",
                NodeData::Decision(DecisionState {
                    status: DecisionStatus::Proposed,
                    selected_option: None,
                    rationale: Some("Easy to serialize".to_owned()),
                    authority_basis: None,
                    reversible: true,
                    decided_by: None,
                }),
                vec![provenance("message-4", 1_700_000_000)],
            ),
        },
        PlanChange::AddNode {
            node: PlanNodeDraft::new(
                question,
                NodeKind::Question,
                "What is the planning interface?",
                "Choose conversation or authored schema",
                NodeData::Question(QuestionState {
                    status: QuestionStatus::Open,
                    impact: QuestionImpact::BlockingNow,
                    decision_owner: rustykrab_projects::DecisionOwner::User,
                    blocking_scope: None,
                    default_action: None,
                    due_milestone: None,
                    resolution: None,
                }),
                vec![provenance("message-4", 1_700_000_000)],
            ),
        },
        PlanChange::AddEdge {
            edge: PlanEdgeDraft::new(
                link,
                old_decision,
                EdgeRelation::Resolves,
                question,
                vec![provenance("message-4", 1_700_000_000)],
            ),
        },
    ]);

    let revised = snapshot
        .apply(PlanChangeSet::new(
            "correction",
            snapshot.project.id,
            snapshot.revision.id.clone(),
            "Use conversation as the interface",
            RevisionAuthor::User,
            timestamp(1_700_000_100),
            vec![PlanChange::SupersedeNode {
                node_id: old_decision,
                replacement: PlanNodeDraft::new(
                    replacement,
                    NodeKind::Decision,
                    "Use a planning conversation",
                    "YAML is only an internal representation",
                    NodeData::Decision(DecisionState {
                        status: DecisionStatus::Accepted,
                        selected_option: None,
                        rationale: Some("Planning is exploratory".to_owned()),
                        authority_basis: Some("Explicit user correction".to_owned()),
                        reversible: true,
                        decided_by: Some(DecisionMaker::User),
                    }),
                    vec![provenance("message-5", 1_700_000_100)],
                ),
            }],
        ))
        .expect("valid correction");

    let historical = revised.revision.nodes.get(&old_decision).unwrap();
    assert!(!historical.is_current());
    assert_eq!(historical.superseded_by, Some(replacement));
    assert_eq!(
        historical.provenance[0].source,
        provenance("message-4", 1_700_000_000).source
    );
    assert!(revised
        .revision
        .nodes
        .get(&replacement)
        .unwrap()
        .is_current());
    assert!(!revised.revision.edges.get(&link).unwrap().is_current());

    let log = revised.revision.decision_log_projection();
    assert_eq!(log.decisions.len(), 2);
    assert!(
        !log.decisions
            .iter()
            .find(|entry| entry.node.id == old_decision)
            .unwrap()
            .current
    );
    let delta = snapshot.revision.compare(&revised.revision).unwrap();
    assert_eq!(delta.added_nodes[0].id, replacement);
    assert_eq!(delta.superseded_nodes[0].id, old_decision);
    assert_eq!(delta.retired_edges[0].id, link);
}

#[test]
fn all_projections_are_consistent_and_deterministic() {
    let intent = generic_node(60, NodeKind::Intent, "Automate delivery", "message-6");
    let outcome = PlanNodeDraft::new(
        node_id(61),
        NodeKind::Outcome,
        "Mergeable changes",
        "Changes arrive as a verified stack",
        NodeData::Outcome(OutcomeState {
            status: OutcomeStatus::Desired,
            success_measures: vec!["PR stack is green".to_owned()],
        }),
        vec![provenance("message-6", 1_700_000_000)],
    );
    let milestone = PlanNodeDraft::new(
        node_id(62),
        NodeKind::Milestone,
        "Durable project state",
        "Planning survives restart",
        NodeData::Milestone(MilestoneState {
            status: MilestoneStatus::InProgress,
            exit_conditions: vec!["Snapshot reloads".to_owned()],
        }),
        vec![provenance("message-6", 1_700_000_000)],
    );
    let risk = PlanNodeDraft::new(
        node_id(63),
        NodeKind::Risk,
        "Untrusted verification",
        "A verifier could approve itself",
        NodeData::Risk(RiskState {
            status: RiskStatus::Open,
            likelihood: RiskLevel::Medium,
            impact: RiskLevel::Critical,
            mitigation: Some("Pin verifier from trusted base".to_owned()),
            trigger: Some("Verification skill changes".to_owned()),
        }),
        vec![provenance("message-7", 1_700_000_000)],
    );
    let behavior = generic_node(
        64,
        NodeKind::AcceptanceBehavior,
        "Restart preserves plan",
        "message-7",
    );
    let observation = generic_node(
        65,
        NodeKind::RepositoryObservation,
        "SQLite store exists",
        "repository:store",
    );
    let snapshot = create_project(vec![
        PlanChange::AddNode { node: intent },
        PlanChange::AddNode { node: outcome },
        PlanChange::AddNode { node: milestone },
        PlanChange::AddNode { node: risk },
        PlanChange::AddNode { node: behavior },
        PlanChange::AddNode { node: observation },
        PlanChange::AddEdge {
            edge: PlanEdgeDraft::new(
                edge_id(66),
                node_id(62),
                EdgeRelation::Advances,
                node_id(61),
                vec![provenance("agent-plan", 1_700_000_000)],
            ),
        },
        PlanChange::AddEdge {
            edge: PlanEdgeDraft::new(
                edge_id(67),
                node_id(64),
                EdgeRelation::Verifies,
                node_id(62),
                vec![provenance("agent-plan", 1_700_000_000)],
            ),
        },
    ]);

    let projections = snapshot.revision.projections();
    let expected = &snapshot.revision.id;
    assert_eq!(&projections.revision_id, expected);
    assert_eq!(&projections.brief.revision_id, expected);
    assert_eq!(&projections.roadmap.revision_id, expected);
    assert_eq!(&projections.decisions.revision_id, expected);
    assert_eq!(&projections.questions.revision_id, expected);
    assert_eq!(&projections.risks.revision_id, expected);
    assert_eq!(&projections.architecture.revision_id, expected);
    assert_eq!(&projections.behavior_catalog.revision_id, expected);
    assert_eq!(&projections.current_understanding.revision_id, expected);
    assert_eq!(projections.brief.intents[0].id, node_id(60));
    assert_eq!(projections.roadmap.milestones[0].node.id, node_id(62));
    assert_eq!(projections.risks.risks[0].node.id, node_id(63));
    assert_eq!(projections.behavior_catalog.behaviors[0].id, node_id(64));
    assert_eq!(
        projections.behavior_catalog.verification_relationships[0].id,
        edge_id(67)
    );
    assert_eq!(projections.current_understanding.nodes.len(), 6);
    assert_eq!(snapshot.revision.projections(), projections);
}

#[test]
fn invalid_typed_states_and_references_are_rejected() {
    let snapshot = create_project(vec![PlanChange::AddNode {
        node: PlanNodeDraft::new(
            node_id(70),
            NodeKind::Question,
            "Resolved how?",
            "A resolution is required",
            NodeData::Question(QuestionState {
                status: QuestionStatus::Open,
                impact: QuestionImpact::Researchable,
                decision_owner: rustykrab_projects::DecisionOwner::Agent,
                blocking_scope: None,
                default_action: None,
                due_milestone: None,
                resolution: None,
            }),
            vec![provenance("message-8", 1_700_000_000)],
        ),
    }]);
    let change = PlanChangeSet::new(
        "bad-resolution",
        snapshot.project.id,
        snapshot.revision.id.clone(),
        "Resolve without an answer",
        RevisionAuthor::Agent,
        timestamp(1_700_000_100),
        vec![PlanChange::UpdateNode {
            node_id: node_id(70),
            patch: PlanNodePatch {
                data: Some(NodeData::Question(QuestionState {
                    status: QuestionStatus::Resolved,
                    impact: QuestionImpact::Researchable,
                    decision_owner: rustykrab_projects::DecisionOwner::Agent,
                    blocking_scope: None,
                    default_action: None,
                    due_milestone: None,
                    resolution: None,
                })),
                ..PlanNodePatch::default()
            },
            provenance: vec![provenance("agent-2", 1_700_000_100)],
        }],
    );
    assert!(matches!(
        snapshot.apply(change),
        Err(ProjectError::InvalidNodeState { .. })
    ));

    let mut command = CreateProject::new(
        "invalid-assumption",
        project_id(2),
        "Invalid project",
        RevisionAuthor::User,
        timestamp(1_700_000_000),
    );
    command.initial_changes = vec![PlanChange::AddNode {
        node: PlanNodeDraft::new(
            node_id(71),
            NodeKind::Assumption,
            "Store is durable",
            "Validate through restart",
            NodeData::Assumption(AssumptionState {
                status: AssumptionStatus::Unvalidated,
                impact: String::new(),
                validation_method: Some("restart test".to_owned()),
            }),
            vec![provenance("message-8", 1_700_000_000)],
        ),
    }];
    assert!(matches!(
        ProjectSnapshot::create(command),
        Err(ProjectError::EmptyField {
            field: "assumption impact"
        })
    ));
}

#[test]
fn project_metadata_changes_are_revisioned_and_hashed() {
    let snapshot = create_project(Vec::new());
    let mut policy = snapshot.project.judgment_policy.clone();
    policy
        .reserved_decisions
        .push("production deployment".to_owned());
    let change = PlanChangeSet::new(
        "update-project-metadata",
        snapshot.project.id,
        snapshot.revision.id.clone(),
        "Attach the durable conversation",
        RevisionAuthor::System,
        timestamp(1_700_000_100),
        Vec::new(),
    )
    .with_project_patch(ProjectPatch {
        provenance: vec![provenance("message-metadata", 1_700_000_100)],
        title: Some("Autonomous software delivery".to_owned()),
        canonical_conversation_id: Some(Some("conversation-2".to_owned())),
        judgment_policy: Some(policy.clone()),
        ..ProjectPatch::default()
    });

    let revised = snapshot.apply(change.clone()).expect("metadata revision");
    let replayed = snapshot.apply(change).expect("deterministic replay");
    assert_eq!(revised, replayed);
    assert_ne!(revised.revision.id, snapshot.revision.id);
    assert_eq!(revised.project.judgment_policy, policy);
    assert_eq!(revised.revision.project_provenance.len(), 1);
    assert_eq!(
        revised.project.canonical_conversation_id.as_deref(),
        Some("conversation-2")
    );

    let untraceable = PlanChangeSet::new(
        "untraceable-metadata",
        snapshot.project.id,
        snapshot.revision.id.clone(),
        "Change authority without a source",
        RevisionAuthor::Agent,
        timestamp(1_700_000_100),
        Vec::new(),
    )
    .with_project_patch(ProjectPatch {
        judgment_policy: Some(JudgmentPolicy {
            statement: "Untraceable authority".to_owned(),
            ..JudgmentPolicy::default()
        }),
        ..ProjectPatch::default()
    });
    assert_eq!(
        snapshot.apply(untraceable),
        Err(ProjectError::MissingProjectProvenance)
    );
}
