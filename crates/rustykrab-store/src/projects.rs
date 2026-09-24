//! Durable storage for conversational project plans.
//!
//! The planning domain owns validation and canonical revision construction.
//! This module owns the persistence guarantees around it: immutable revisions,
//! optimistic current-revision checks, request replay, and atomic materialized
//! node/edge indexes. Conversation text stays in the conversation store; only
//! stable conversation and message identifiers are recorded here.

use std::sync::{Arc, Mutex};

use rusqlite::{params, OptionalExtension, Transaction};
use serde::{de::DeserializeOwned, Serialize};

use rustykrab_core::Error;
use rustykrab_projects::{
    CreateProject, PlanChangeSet, Project, ProjectId, ProjectRevision, ProjectSnapshot, RevisionId,
};

use crate::with_conn;

/// Receipt for a create or revision application.
///
/// `replayed` is true when the request id was already committed. The returned
/// snapshot is the exact snapshot produced by that original request, even if
/// the project has advanced since then.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyRevisionResult {
    pub snapshot: ProjectSnapshot,
    pub replayed: bool,
}

/// Handle for durable project-planning operations.
#[derive(Clone)]
pub struct ProjectStore {
    conn: Arc<Mutex<rusqlite::Connection>>,
}

impl ProjectStore {
    pub(crate) fn new(conn: Arc<Mutex<rusqlite::Connection>>) -> Self {
        Self { conn }
    }

    /// Create a project and its root revision in one transaction.
    ///
    /// Reusing a request id with the same payload returns the original result.
    /// Reusing it with different content is rejected rather than ambiguously
    /// claiming that the second request succeeded.
    pub async fn create(&self, request: CreateProject) -> Result<ApplyRevisionResult, Error> {
        let request_data = to_json(&request)?;
        with_conn(&self.conn, move |conn| {
            let tx = conn.unchecked_transaction().map_err(storage_error)?;

            if let Some((stored_request, snapshot)) = load_create_replay(&tx, &request.request_id)?
            {
                ensure_same_request(&stored_request, &request_data, &request.request_id)?;
                return Ok(ApplyRevisionResult {
                    snapshot,
                    replayed: true,
                });
            }

            let snapshot = ProjectSnapshot::create(request).map_err(project_error)?;
            insert_project(&tx, &snapshot, &request_data)?;
            insert_revision(&tx, &snapshot, &request_data)?;
            set_current_revision(&tx, &snapshot, None)?;
            tx.commit().map_err(storage_error)?;

            Ok(ApplyRevisionResult {
                snapshot,
                replayed: false,
            })
        })
        .await
    }

    /// Apply one immutable revision using optimistic parent/current checking.
    ///
    /// Domain validation, revision insertion, node/edge materialization, and
    /// the current pointer update all happen while one SQLite transaction is
    /// open. Any error therefore leaves no partial revision behind.
    pub async fn apply(
        &self,
        project_id: &ProjectId,
        expected_parent: &RevisionId,
        change_set: PlanChangeSet,
    ) -> Result<ApplyRevisionResult, Error> {
        let project_id = *project_id;
        let expected_parent = expected_parent.clone();
        let request_data = to_json(&change_set)?;

        with_conn(&self.conn, move |conn| {
            let tx = conn.unchecked_transaction().map_err(storage_error)?;

            if let Some((stored_request, snapshot)) =
                load_revision_replay(&tx, &project_id, &change_set.request_id)?
            {
                ensure_same_request(&stored_request, &request_data, &change_set.request_id)?;
                return Ok(ApplyRevisionResult {
                    snapshot,
                    replayed: true,
                });
            }

            let current = load_current(&tx, &project_id)?
                .ok_or_else(|| Error::NotFound(format!("project {project_id}")))?;
            if current.revision.id != expected_parent {
                return Err(revision_conflict(
                    &project_id,
                    &expected_parent,
                    &current.revision.id,
                ));
            }

            let next = current.apply(change_set).map_err(project_error)?;
            insert_revision(&tx, &next, &request_data)?;
            set_current_revision(&tx, &next, Some(&expected_parent))?;
            tx.commit().map_err(storage_error)?;

            Ok(ApplyRevisionResult {
                snapshot: next,
                replayed: false,
            })
        })
        .await
    }

    /// Load the current immutable project snapshot.
    pub async fn get(&self, project_id: &ProjectId) -> Result<Option<ProjectSnapshot>, Error> {
        let project_id = *project_id;
        with_conn(&self.conn, move |conn| load_current(conn, &project_id)).await
    }

    /// Load the original create command for an idempotency key.
    ///
    /// HTTP and other adapters use this to reuse server-generated values such
    /// as the creation timestamp when reconstructing a retry. They must still
    /// pass the reconstructed command to [`Self::create`], which compares the
    /// complete serialized request and rejects any changed caller input.
    pub async fn get_create_request(
        &self,
        request_id: &str,
    ) -> Result<Option<CreateProject>, Error> {
        let request_id = request_id.to_owned();
        with_conn(&self.conn, move |conn| {
            let request = conn
                .query_row(
                    "SELECT create_request FROM projects WHERE create_request_id = ?1",
                    params![request_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(storage_error)?;
            request.map(|request| from_json(&request)).transpose()
        })
        .await
    }

    /// Load the original revision command for an idempotency key.
    ///
    /// Transport adapters use this to reuse server-derived values such as the
    /// author and timestamp when reconstructing an exact retry. The command
    /// must still be passed to [`Self::apply`], which compares its complete
    /// serialization and rejects changed caller input under the same key.
    pub async fn get_revision_request(
        &self,
        project_id: &ProjectId,
        request_id: &str,
    ) -> Result<Option<PlanChangeSet>, Error> {
        let project_id = *project_id;
        let request_id = request_id.to_owned();
        with_conn(&self.conn, move |conn| {
            let request = conn
                .query_row(
                    "SELECT request_data FROM project_revisions
                     WHERE project_id = ?1 AND request_id = ?2",
                    params![project_id.to_string(), request_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(storage_error)?;
            request.map(|request| from_json(&request)).transpose()
        })
        .await
    }

    /// Load the exact project snapshot produced by a historical revision.
    pub async fn get_revision(
        &self,
        project_id: &ProjectId,
        revision_id: &RevisionId,
    ) -> Result<Option<ProjectSnapshot>, Error> {
        let project_id = *project_id;
        let revision_id = revision_id.clone();
        with_conn(&self.conn, move |conn| {
            load_revision(conn, &project_id, &revision_id)
        })
        .await
    }

    /// List project metadata ordered by most recent activity.
    pub async fn list(&self) -> Result<Vec<Project>, Error> {
        with_conn(&self.conn, |conn| {
            let mut stmt = conn
                .prepare("SELECT data FROM projects ORDER BY updated_at DESC, id")
                .map_err(storage_error)?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(storage_error)?;
            let mut projects = Vec::new();
            for row in rows {
                projects.push(from_json(&row.map_err(storage_error)?)?);
            }
            Ok(projects)
        })
        .await
    }

    /// List immutable revisions in sequence order.
    pub async fn list_revisions(
        &self,
        project_id: &ProjectId,
    ) -> Result<Vec<ProjectRevision>, Error> {
        let project_id = *project_id;
        with_conn(&self.conn, move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT data FROM project_revisions
                     WHERE project_id = ?1 ORDER BY sequence",
                )
                .map_err(storage_error)?;
            let rows = stmt
                .query_map(params![project_id.to_string()], |row| {
                    row.get::<_, String>(0)
                })
                .map_err(storage_error)?;
            let mut revisions = Vec::new();
            for row in rows {
                revisions.push(from_json(&row.map_err(storage_error)?)?);
            }
            Ok(revisions)
        })
        .await
    }

    pub async fn revision_count(&self, project_id: &ProjectId) -> Result<u64, Error> {
        let project_id = *project_id;
        with_conn(&self.conn, move |conn| {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM project_revisions WHERE project_id = ?1",
                    params![project_id.to_string()],
                    |row| row.get(0),
                )
                .map_err(storage_error)?;
            u64::try_from(count).map_err(|_| Error::Storage("negative revision count".into()))
        })
        .await
    }
}

fn insert_project(
    tx: &Transaction<'_>,
    snapshot: &ProjectSnapshot,
    create_request: &str,
) -> Result<(), Error> {
    let project = &snapshot.project;
    tx.execute(
        "INSERT INTO projects
            (id, create_request_id, repository_id, canonical_conversation_id,
             title, status, judgment_policy, current_revision, create_request,
             data, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, ?8, ?9, ?10, ?11)",
        params![
            project.id.to_string(),
            snapshot.revision.request_id,
            project.repository_id,
            project.canonical_conversation_id,
            project.title,
            to_json(&project.status)?,
            to_json(&project.judgment_policy)?,
            create_request,
            to_json(project)?,
            project.created_at.to_rfc3339(),
            project.updated_at.to_rfc3339(),
        ],
    )
    .map_err(storage_error)?;
    Ok(())
}

fn insert_revision(
    tx: &Transaction<'_>,
    snapshot: &ProjectSnapshot,
    request_data: &str,
) -> Result<(), Error> {
    let revision = &snapshot.revision;
    let sequence = i64::try_from(revision.sequence)
        .map_err(|_| Error::Storage("project revision sequence exceeds SQLite range".into()))?;
    let (conversation_id, source_message_id) = revision
        .source_message
        .as_ref()
        .map(|source| {
            (
                Some(source.conversation_id.as_str()),
                Some(source.message_id.as_str()),
            )
        })
        .unwrap_or((None, None));

    tx.execute(
        "INSERT INTO project_revisions
            (id, project_id, parent_revision, sequence, request_id, request_data,
             author, conversation_id, source_message_id, summary, project_data,
             data, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            revision.id.to_string(),
            revision.project_id.to_string(),
            revision.parent_revision.as_ref().map(ToString::to_string),
            sequence,
            revision.request_id,
            request_data,
            to_json(&revision.author)?,
            conversation_id,
            source_message_id,
            revision.summary,
            to_json(&snapshot.project)?,
            to_json(revision)?,
            revision.created_at.to_rfc3339(),
        ],
    )
    .map_err(storage_error)?;

    {
        let mut stmt = tx
            .prepare(
                "INSERT INTO plan_nodes
                    (revision_id, project_id, id, kind, data)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .map_err(storage_error)?;
        for node in revision.nodes.values() {
            stmt.execute(params![
                revision.id.to_string(),
                revision.project_id.to_string(),
                node.id.to_string(),
                node.kind.as_str(),
                to_json(node)?,
            ])
            .map_err(storage_error)?;
        }
    }

    {
        let mut stmt = tx
            .prepare(
                "INSERT INTO plan_edges
                    (revision_id, project_id, id, from_node, relation, to_node, data)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )
            .map_err(storage_error)?;
        for edge in revision.edges.values() {
            stmt.execute(params![
                revision.id.to_string(),
                revision.project_id.to_string(),
                edge.id.to_string(),
                edge.from.to_string(),
                edge.relation.as_str(),
                edge.to.to_string(),
                to_json(edge)?,
            ])
            .map_err(storage_error)?;
        }
    }

    Ok(())
}

fn set_current_revision(
    tx: &Transaction<'_>,
    snapshot: &ProjectSnapshot,
    expected_parent: Option<&RevisionId>,
) -> Result<(), Error> {
    let project = &snapshot.project;
    let changed = match expected_parent {
        Some(parent) => tx.execute(
            "UPDATE projects
             SET current_revision = ?1, repository_id = ?2,
                 canonical_conversation_id = ?3, title = ?4, status = ?5,
                 judgment_policy = ?6, data = ?7, updated_at = ?8
             WHERE id = ?9 AND current_revision = ?10",
            params![
                snapshot.revision.id.to_string(),
                project.repository_id,
                project.canonical_conversation_id,
                project.title,
                to_json(&project.status)?,
                to_json(&project.judgment_policy)?,
                to_json(project)?,
                project.updated_at.to_rfc3339(),
                project.id.to_string(),
                parent.to_string(),
            ],
        ),
        None => tx.execute(
            "UPDATE projects SET current_revision = ?1 WHERE id = ?2 AND current_revision IS NULL",
            params![snapshot.revision.id.to_string(), project.id.to_string()],
        ),
    }
    .map_err(storage_error)?;

    if changed != 1 {
        return Err(Error::Storage(format!(
            "project {} current revision changed concurrently",
            project.id
        )));
    }
    Ok(())
}

fn load_current(
    conn: &rusqlite::Connection,
    project_id: &ProjectId,
) -> Result<Option<ProjectSnapshot>, Error> {
    let row = conn
        .query_row(
            "SELECT r.project_data, r.data
             FROM projects p
             JOIN project_revisions r ON r.id = p.current_revision
             WHERE p.id = ?1",
            params![project_id.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(storage_error)?;
    row.map(parse_snapshot).transpose()
}

fn load_revision(
    conn: &rusqlite::Connection,
    project_id: &ProjectId,
    revision_id: &RevisionId,
) -> Result<Option<ProjectSnapshot>, Error> {
    let row = conn
        .query_row(
            "SELECT project_data, data FROM project_revisions
             WHERE project_id = ?1 AND id = ?2",
            params![project_id.to_string(), revision_id.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(storage_error)?;
    row.map(parse_snapshot).transpose()
}

fn load_create_replay(
    conn: &rusqlite::Connection,
    request_id: &str,
) -> Result<Option<(String, ProjectSnapshot)>, Error> {
    let row = conn
        .query_row(
            "SELECT p.create_request, r.project_data, r.data
             FROM projects p
             JOIN project_revisions r
               ON r.project_id = p.id AND r.request_id = p.create_request_id
             WHERE p.create_request_id = ?1",
            params![request_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()
        .map_err(storage_error)?;
    row.map(|(request, project, revision)| Ok((request, parse_snapshot((project, revision))?)))
        .transpose()
}

fn load_revision_replay(
    conn: &rusqlite::Connection,
    project_id: &ProjectId,
    request_id: &str,
) -> Result<Option<(String, ProjectSnapshot)>, Error> {
    let row = conn
        .query_row(
            "SELECT request_data, project_data, data FROM project_revisions
             WHERE project_id = ?1 AND request_id = ?2",
            params![project_id.to_string(), request_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()
        .map_err(storage_error)?;
    row.map(|(request, project, revision)| Ok((request, parse_snapshot((project, revision))?)))
        .transpose()
}

fn parse_snapshot((project, revision): (String, String)) -> Result<ProjectSnapshot, Error> {
    Ok(ProjectSnapshot {
        project: from_json(&project)?,
        revision: from_json(&revision)?,
    })
}

fn ensure_same_request(stored: &str, received: &str, request_id: &str) -> Result<(), Error> {
    if stored == received {
        Ok(())
    } else {
        Err(Error::AlreadyExists(format!(
            "project request id {request_id} was already used with different content"
        )))
    }
}

fn revision_conflict(project_id: &ProjectId, expected: &RevisionId, actual: &RevisionId) -> Error {
    Error::Storage(format!(
        "project {project_id} revision conflict: expected {expected}, current is {actual}"
    ))
}

fn project_error(error: rustykrab_projects::ProjectError) -> Error {
    Error::Storage(format!("invalid project revision: {error}"))
}

fn storage_error(error: rusqlite::Error) -> Error {
    Error::Storage(error.to_string())
}

fn to_json(value: &impl Serialize) -> Result<String, Error> {
    serde_json::to_string(value).map_err(Error::from)
}

fn from_json<T: DeserializeOwned>(value: &str) -> Result<T, Error> {
    serde_json::from_str(value).map_err(Error::from)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use rusqlite::params;
    use tempfile::TempDir;

    use rustykrab_core::types::{Message, MessageContent, Role};
    use rustykrab_projects::{
        CreateProject, DecisionMaker, DecisionOwner, DecisionState, DecisionStatus, EdgeId,
        EdgeRelation, JudgmentPolicy, MessageRef, NodeData, NodeId, NodeKind, OutcomeState,
        OutcomeStatus, PlanChange, PlanChangeSet, PlanEdgeDraft, PlanNodeDraft, ProjectId,
        ProjectPatch, Provenance, ProvenanceClassification, ProvenanceSource, QuestionImpact,
        QuestionState, QuestionStatus, RevisionAuthor,
    };

    use crate::Store;

    fn open(dir: &TempDir) -> Store {
        Store::open(dir.path(), vec![7; 32]).unwrap()
    }

    fn provenance() -> Provenance {
        Provenance {
            classification: ProvenanceClassification::UserStated,
            source: ProvenanceSource::ConversationMessage {
                conversation_id: "conversation-1".to_owned(),
                message_id: "message-1".to_owned(),
            },
            recorded_at: Utc::now(),
            confidence: None,
            freshness: None,
        }
    }

    fn create_command(request_id: &str) -> CreateProject {
        let mut command = CreateProject::new(
            request_id,
            ProjectId::new(),
            "Durable planning",
            RevisionAuthor::User,
            Utc::now(),
        );
        command.repository_id = Some("gcbh/rustykrab".to_owned());
        command.canonical_conversation_id = Some("conversation-1".to_owned());
        command.source_message = Some(MessageRef::new("conversation-1", "message-1").unwrap());
        command.judgment_policy = JudgmentPolicy {
            statement: "Decide reversible implementation details".to_owned(),
            delegated_scopes: vec!["internal implementation".to_owned()],
            reserved_decisions: vec!["security boundary".to_owned()],
            ..JudgmentPolicy::default()
        };
        command
    }

    fn add_generic_node(
        request_id: &str,
        project_id: ProjectId,
        parent: rustykrab_projects::RevisionId,
    ) -> PlanChangeSet {
        PlanChangeSet::new(
            request_id,
            project_id,
            parent,
            "Add an outcome",
            RevisionAuthor::Agent,
            Utc::now(),
            vec![PlanChange::AddNode {
                node: PlanNodeDraft::new(
                    NodeId::new(),
                    NodeKind::Outcome,
                    "Planning survives restart",
                    "The current project model can be reconstructed.",
                    NodeData::Outcome(OutcomeState {
                        status: OutcomeStatus::Desired,
                        success_measures: vec!["snapshot round-trips exactly".to_owned()],
                    }),
                    vec![provenance()],
                ),
            }],
        )
    }

    #[tokio::test]
    async fn reconstructs_project_decisions_questions_and_links_after_reopen() {
        let dir = TempDir::new().unwrap();
        let store = open(&dir);
        let mut command = create_command("create-restart");
        let question_id = NodeId::new();
        let decision_id = NodeId::new();
        let source = provenance();
        command.initial_changes = vec![
            PlanChange::AddNode {
                node: PlanNodeDraft::new(
                    question_id,
                    NodeKind::Question,
                    "Where should transcripts live?",
                    "Avoid duplicating conversation content.",
                    NodeData::Question(QuestionState {
                        status: QuestionStatus::Resolved,
                        impact: QuestionImpact::BlockingNow,
                        decision_owner: DecisionOwner::User,
                        blocking_scope: None,
                        default_action: None,
                        due_milestone: None,
                        resolution: Some("Keep them in the conversation store".to_owned()),
                    }),
                    vec![source.clone()],
                ),
            },
            PlanChange::AddNode {
                node: PlanNodeDraft::new(
                    decision_id,
                    NodeKind::Decision,
                    "Link messages by identifier",
                    "Project provenance holds references, not message text.",
                    NodeData::Decision(DecisionState {
                        status: DecisionStatus::Accepted,
                        selected_option: None,
                        rationale: Some("The conversation store is authoritative".to_owned()),
                        authority_basis: Some("Explicit user direction".to_owned()),
                        reversible: true,
                        decided_by: Some(DecisionMaker::User),
                    }),
                    vec![source.clone()],
                ),
            },
            PlanChange::AddEdge {
                edge: PlanEdgeDraft::new(
                    EdgeId::new(),
                    decision_id,
                    EdgeRelation::Resolves,
                    question_id,
                    vec![source],
                ),
            },
        ];

        let original_command = command.clone();
        let created = store.projects().create(command).await.unwrap();
        let root = created.snapshot.clone();
        let revised_policy = JudgmentPolicy {
            statement: "Decide reversible internal details without new dependencies".to_owned(),
            delegated_scopes: vec!["internal implementation".to_owned()],
            reserved_decisions: vec!["security boundary".to_owned(), "new dependency".to_owned()],
            ..JudgmentPolicy::default()
        };
        let metadata_change = PlanChangeSet::new(
            "update-policy",
            root.project.id,
            root.revision.id.clone(),
            "Narrow delegated judgment",
            RevisionAuthor::User,
            Utc::now(),
            Vec::new(),
        )
        .with_source_message(MessageRef::new("conversation-1", "message-2").unwrap())
        .with_project_patch(ProjectPatch {
            provenance: vec![provenance()],
            judgment_policy: Some(revised_policy.clone()),
            ..ProjectPatch::default()
        });
        let expected = store
            .projects()
            .apply(&root.project.id, &root.revision.id, metadata_change)
            .await
            .unwrap()
            .snapshot;
        let project_id = expected.project.id;
        assert_eq!(expected.project.judgment_policy, revised_policy);
        assert_eq!(
            store.projects().revision_count(&project_id).await.unwrap(),
            2
        );
        drop(created);
        drop(store);

        let reopened = open(&dir);
        assert_eq!(
            reopened
                .projects()
                .get_create_request("create-restart")
                .await
                .unwrap(),
            Some(original_command)
        );
        assert!(reopened
            .projects()
            .get_create_request("missing-request")
            .await
            .unwrap()
            .is_none());
        let restored = reopened.projects().get(&project_id).await.unwrap().unwrap();
        assert_eq!(restored, expected);
        assert_eq!(
            restored.project.judgment_policy,
            expected.project.judgment_policy
        );
        assert_eq!(
            restored
                .revision
                .source_message
                .as_ref()
                .map(|source| (source.conversation_id.as_str(), source.message_id.as_str())),
            Some(("conversation-1", "message-2"))
        );
        assert_eq!(restored.revision.nodes.len(), 2);
        assert_eq!(restored.revision.edges.len(), 1);
        assert_eq!(restored.revision.project_provenance.len(), 1);
        assert_eq!(
            reopened
                .projects()
                .revision_count(&project_id)
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            reopened
                .projects()
                .get_revision(&project_id, &root.revision.id)
                .await
                .unwrap()
                .unwrap(),
            root
        );
    }

    #[tokio::test]
    async fn duplicate_request_ids_replay_exact_results_without_new_revisions() {
        let dir = TempDir::new().unwrap();
        let store = open(&dir);
        let command = create_command("create-replay");
        let first = store.projects().create(command.clone()).await.unwrap();

        let change_set = add_generic_node(
            "apply-replay",
            first.snapshot.project.id,
            first.snapshot.revision.id.clone(),
        );
        let applied = store
            .projects()
            .apply(
                &first.snapshot.project.id,
                &first.snapshot.revision.id,
                change_set.clone(),
            )
            .await
            .unwrap();

        // Advance once more before retrying either earlier request. Replay
        // must return each request's historical result, not today's head.
        let later_change = add_generic_node(
            "apply-later",
            first.snapshot.project.id,
            applied.snapshot.revision.id.clone(),
        );
        store
            .projects()
            .apply(
                &first.snapshot.project.id,
                &applied.snapshot.revision.id,
                later_change,
            )
            .await
            .unwrap();

        let repeated = store
            .projects()
            .apply(
                &first.snapshot.project.id,
                &first.snapshot.revision.id,
                change_set.clone(),
            )
            .await
            .unwrap();
        assert!(repeated.replayed);
        assert_eq!(repeated.snapshot, applied.snapshot);
        assert_eq!(
            store
                .projects()
                .get_revision_request(&first.snapshot.project.id, "apply-replay")
                .await
                .unwrap(),
            Some(change_set.clone())
        );
        assert!(store
            .projects()
            .get_revision_request(&first.snapshot.project.id, "missing-request")
            .await
            .unwrap()
            .is_none());

        let create_replay = store.projects().create(command).await.unwrap();
        assert!(create_replay.replayed);
        assert_eq!(create_replay.snapshot, first.snapshot);

        let mut conflicting_request = change_set;
        conflicting_request.summary = "Different content under the same request id".to_owned();
        let conflict = store
            .projects()
            .apply(
                &first.snapshot.project.id,
                &first.snapshot.revision.id,
                conflicting_request,
            )
            .await
            .unwrap_err();
        assert!(matches!(conflict, rustykrab_core::Error::AlreadyExists(_)));
        assert_eq!(
            store
                .projects()
                .revision_count(&first.snapshot.project.id)
                .await
                .unwrap(),
            3
        );
    }

    #[tokio::test]
    async fn rejects_a_stale_parent_without_writing_a_revision() {
        let dir = TempDir::new().unwrap();
        let store = open(&dir);
        let root = store
            .projects()
            .create(create_command("create-stale"))
            .await
            .unwrap()
            .snapshot;
        let first_change =
            add_generic_node("apply-first", root.project.id, root.revision.id.clone());
        store
            .projects()
            .apply(&root.project.id, &root.revision.id, first_change)
            .await
            .unwrap();

        let stale = add_generic_node("apply-stale", root.project.id, root.revision.id.clone());
        let error = store
            .projects()
            .apply(&root.project.id, &root.revision.id, stale)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("revision conflict"));
        assert_eq!(
            store
                .projects()
                .revision_count(&root.project.id)
                .await
                .unwrap(),
            2
        );
    }

    #[tokio::test]
    async fn invalid_change_set_is_atomic() {
        let dir = TempDir::new().unwrap();
        let store = open(&dir);
        let root = store
            .projects()
            .create(create_command("create-atomic"))
            .await
            .unwrap()
            .snapshot;
        let before = store
            .projects()
            .get(&root.project.id)
            .await
            .unwrap()
            .unwrap();
        let invalid = PlanChangeSet::new(
            "apply-invalid",
            root.project.id,
            root.revision.id.clone(),
            "Add an invalid edge",
            RevisionAuthor::Agent,
            Utc::now(),
            vec![PlanChange::AddEdge {
                edge: PlanEdgeDraft::new(
                    EdgeId::new(),
                    NodeId::new(),
                    EdgeRelation::Supports,
                    NodeId::new(),
                    vec![provenance()],
                ),
            }],
        );

        store
            .projects()
            .apply(&root.project.id, &root.revision.id, invalid)
            .await
            .unwrap_err();
        assert_eq!(
            store
                .projects()
                .revision_count(&root.project.id)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .projects()
                .get(&root.project.id)
                .await
                .unwrap()
                .unwrap(),
            before
        );
    }

    #[tokio::test]
    async fn sqlite_rejects_cross_project_revision_and_materialization_links() {
        let dir = TempDir::new().unwrap();
        let store = open(&dir);
        let first = store
            .projects()
            .create(create_command("create-first-project"))
            .await
            .unwrap()
            .snapshot;
        let second = store
            .projects()
            .create(create_command("create-second-project"))
            .await
            .unwrap()
            .snapshot;

        let projects = store.projects();
        let conn = projects.conn.lock().unwrap();
        let current_revision_error = conn
            .execute(
                "UPDATE projects SET current_revision = ?1 WHERE id = ?2",
                params![second.revision.id.to_string(), first.project.id.to_string()],
            )
            .unwrap_err();
        assert!(current_revision_error.to_string().contains("FOREIGN KEY"));

        let parent_revision_error = conn
            .execute(
                "INSERT INTO project_revisions
                    (id, project_id, parent_revision, sequence, request_id,
                     request_data, author, summary, project_data, data, created_at)
                 VALUES (?1, ?2, ?3, 2, ?4, '{}', 'agent', 'forged', '{}', '{}', ?5)",
                params![
                    "forged-cross-project-revision",
                    first.project.id.to_string(),
                    second.revision.id.to_string(),
                    "forged-cross-project-request",
                    Utc::now().to_rfc3339(),
                ],
            )
            .unwrap_err();
        assert!(parent_revision_error.to_string().contains("FOREIGN KEY"));

        let node_error = conn
            .execute(
                "INSERT INTO plan_nodes (revision_id, project_id, id, kind, data)
                 VALUES (?1, ?2, 'forged-cross-project-node', 'outcome', '{}')",
                params![first.revision.id.to_string(), second.project.id.to_string()],
            )
            .unwrap_err();
        assert!(node_error.to_string().contains("FOREIGN KEY"));

        let edge_error = conn
            .execute(
                "INSERT INTO plan_edges
                    (revision_id, project_id, id, from_node, relation, to_node, data)
                 VALUES (?1, ?2, 'forged-cross-project-edge', 'missing-a',
                         'supports', 'missing-b', '{}')",
                params![first.revision.id.to_string(), second.project.id.to_string()],
            )
            .unwrap_err();
        assert!(edge_error.to_string().contains("FOREIGN KEY"));

        let violations: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(violations, 0);
    }

    #[tokio::test]
    async fn provenance_identifiers_survive_conversation_deletion_without_copying_text() {
        let dir = TempDir::new().unwrap();
        let store = open(&dir);
        let mut conversation = store
            .conversations()
            .create_with_title(Some("Planning source".to_owned()))
            .await
            .unwrap();
        let message = Message {
            id: uuid::Uuid::new_v4(),
            role: Role::User,
            content: MessageContent::Text("Keep durable provenance by identifier".to_owned()),
            created_at: Utc::now(),
            agent_version: None,
        };
        conversation.messages.push(message.clone());
        store.conversations().save(&conversation).await.unwrap();

        let mut command = create_command("create-with-real-source");
        command.canonical_conversation_id = Some(conversation.id.to_string());
        command.source_message =
            Some(MessageRef::new(conversation.id.to_string(), message.id.to_string()).unwrap());
        let created = store.projects().create(command).await.unwrap().snapshot;

        store.conversations().delete(conversation.id).await.unwrap();
        let restored = store
            .projects()
            .get(&created.project.id)
            .await
            .unwrap()
            .unwrap();
        let source = restored.revision.source_message.as_ref().unwrap();
        assert_eq!(source.conversation_id, conversation.id.to_string());
        assert_eq!(source.message_id, message.id.to_string());

        let projects = store.projects();
        let conn = projects.conn.lock().unwrap();
        let conversation_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM conversations WHERE id = ?1",
                params![conversation.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        let message_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE conversation_id = ?1",
                params![conversation.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!((conversation_rows, message_rows), (0, 0));
        let revision_json = serde_json::to_string(&restored.revision).unwrap();
        assert!(!revision_json.contains("Keep durable provenance by identifier"));
    }
}
