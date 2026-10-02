//! HTTP surface for the durable project-planning model.
//!
//! These handlers expose deterministic domain commands and projections. They
//! deliberately do not invoke a model or grant repository, GitHub, merge, or
//! deployment authority; conversational interpretation arrives in a later
//! construction stack.

use std::collections::{BTreeMap, BTreeSet};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use rustykrab_core::Error;
use rustykrab_projects::{
    AssumptionState, AssumptionStatus, CreateProject, EdgeId, EdgeRelation, JudgmentPolicy,
    MessageRef, NodeData, NodeId, NodeKind, PlanChange, PlanChangeSet, PlanEdgeDraft,
    PlanNodeDraft, PlanNodePatch, ProjectId, ProjectPatch, ProjectSnapshot, ProjectStatus,
    ProjectionKind, Provenance, ProvenanceClassification, ProvenanceSource, RevisionAuthor,
    RevisionId,
};

use crate::AppState;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/projects", post(create_project).get(list_projects))
        .route("/api/projects/{project_id}", get(inspect_project))
        .route("/api/projects/{project_id}/snapshot", get(inspect_project))
        .route(
            "/api/projects/{project_id}/revisions",
            get(list_revisions).post(apply_revision),
        )
        .route(
            "/api/projects/{project_id}/revisions/{revision_id}",
            get(inspect_revision),
        )
        .route("/api/projects/{project_id}/compare", get(compare_revisions))
        .route(
            "/api/projects/{project_id}/projections",
            get(project_projections),
        )
        .route(
            "/api/projects/{project_id}/projections/{kind}",
            get(project_projection),
        )
}

#[derive(Clone, Debug, Deserialize)]
struct CreateProjectRequest {
    #[serde(default)]
    request_id: Option<String>,
    #[serde(default)]
    project_id: Option<Uuid>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    repository_id: Option<String>,
    #[serde(default)]
    repository_path: Option<String>,
    #[serde(default)]
    base_revision: Option<String>,
    #[serde(default)]
    canonical_conversation_id: Option<String>,
    #[serde(default)]
    source_message_id: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    intent: Option<String>,
    #[serde(default)]
    judgment_policy: Option<JudgmentPolicy>,
}

/// External revision DTO. It deliberately has no author, timestamp, or
/// provenance-classification fields; those are facts minted by the service.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplyRevisionRequest {
    #[serde(default)]
    request_id: Option<String>,
    parent_revision: RevisionId,
    summary: String,
    #[serde(default)]
    source_message: Option<MessageRef>,
    #[serde(default)]
    project_patch: Option<ProjectPatchRequest>,
    #[serde(default)]
    changes: Vec<PlanChangeRequest>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectPatchRequest {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    status: Option<ProjectStatus>,
    #[serde(default)]
    repository_id: Option<String>,
    #[serde(default)]
    canonical_conversation_id: Option<String>,
    #[serde(default)]
    judgment_policy: Option<JudgmentPolicy>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, tag = "operation", rename_all = "snake_case")]
enum PlanChangeRequest {
    AddNode {
        node: PlanNodeRequest,
    },
    UpdateNode {
        node_id: NodeId,
        patch: PlanNodePatch,
    },
    SupersedeNode {
        node_id: NodeId,
        replacement: PlanNodeRequest,
    },
    AddEdge {
        edge: PlanEdgeRequest,
    },
    RemoveEdge {
        edge_id: EdgeId,
    },
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanNodeRequest {
    id: NodeId,
    kind: NodeKind,
    title: String,
    body: String,
    data: NodeData,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanEdgeRequest {
    id: EdgeId,
    from: NodeId,
    relation: EdgeRelation,
    to: NodeId,
}

#[derive(Debug, Deserialize, Default)]
struct RevisionQuery {
    #[serde(default)]
    revision: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CompareQuery {
    from: String,
    to: String,
}

#[derive(Debug, Serialize)]
struct ApiErrorBody {
    error: &'static str,
    message: String,
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "invalid_request",
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "not_found",
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ApiErrorBody {
                error: self.code,
                message: self.message,
            }),
        )
            .into_response()
    }
}

impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        match error {
            Error::NotFound(message) => Self::not_found(message),
            Error::AlreadyExists(message) => Self {
                status: StatusCode::CONFLICT,
                code: "request_conflict",
                message,
            },
            Error::Storage(message) if message.contains("revision conflict") => Self {
                status: StatusCode::CONFLICT,
                code: "revision_conflict",
                message,
            },
            Error::Storage(message) if message.starts_with("invalid project revision:") => Self {
                status: StatusCode::UNPROCESSABLE_ENTITY,
                code: "invalid_revision",
                message,
            },
            other => {
                tracing::error!(error = %other, "project API operation failed");
                Self {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    code: "internal_error",
                    message: "project operation failed".to_owned(),
                }
            }
        }
    }
}

async fn create_project(
    State(state): State<AppState>,
    Json(body): Json<CreateProjectRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let request_id = body
        .request_id
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    // `created_at` and provenance timestamps are server-generated. Reusing
    // the original value lets the store's exact command comparison distinguish
    // a genuine retry from changed content under the same request id.
    let stored_request = state
        .agent
        .store
        .projects()
        .get_create_request(&request_id)
        .await?;
    let created_at = stored_request
        .as_ref()
        .map(|request| request.created_at)
        .unwrap_or_else(Utc::now);
    let command = create_project_command(body.clone(), request_id.clone(), created_at)?;

    let result = match state.agent.store.projects().create(command).await {
        Ok(result) => result,
        // A concurrent first request may have committed after our lookup.
        // Reconstruct once from its timestamp; changed client content still
        // fails the store's byte-for-byte request comparison.
        Err(Error::AlreadyExists(_)) if stored_request.is_none() => {
            let stored = state
                .agent
                .store
                .projects()
                .get_create_request(&request_id)
                .await?
                .ok_or_else(|| {
                    ApiError::from(Error::AlreadyExists(format!(
                        "project request id {request_id} was committed concurrently"
                    )))
                })?;
            let command = create_project_command(body, request_id, stored.created_at)?;
            state.agent.store.projects().create(command).await?
        }
        Err(error) => return Err(error.into()),
    };
    let status = if result.replayed {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    let response = snapshot_response(&state, &result.snapshot, Some(result.replayed)).await?;
    Ok((status, Json(response)))
}

fn create_project_command(
    body: CreateProjectRequest,
    request_id: String,
    created_at: chrono::DateTime<Utc>,
) -> Result<CreateProject, ApiError> {
    let intent = body
        .message
        .or(body.intent)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| ApiError::bad_request("message or intent must not be empty"))?;
    let base_revision = body
        .base_revision
        .map(|value| {
            if value.trim().is_empty() {
                Err(ApiError::bad_request("base_revision must not be empty"))
            } else {
                Ok(value)
            }
        })
        .transpose()?;
    let source_message = source_message(
        body.canonical_conversation_id.as_deref(),
        body.source_message_id.as_deref(),
    )?;
    let provenance = source_message
        .as_ref()
        .map(|source| {
            Provenance::conversation(ProvenanceClassification::UserStated, source, created_at)
        })
        .unwrap_or_else(|| Provenance {
            classification: ProvenanceClassification::UserStated,
            source: ProvenanceSource::Manual {
                reference: format!("project create request {request_id}"),
            },
            recorded_at: created_at,
            confidence: None,
            freshness: None,
        });

    let title = body
        .title
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| title_from_intent(&intent));
    let project_id = body
        .project_id
        .map(ProjectId::from_uuid)
        .unwrap_or_else(|| project_id_for_request(&request_id));
    let mut command = CreateProject::new(
        request_id.clone(),
        project_id,
        title,
        RevisionAuthor::User,
        created_at,
    );
    command.repository_id = body.repository_id.or(body.repository_path);
    command.canonical_conversation_id = body.canonical_conversation_id;
    command.source_message = source_message;
    command.project_provenance = vec![provenance.clone()];
    command.judgment_policy = body.judgment_policy.unwrap_or_default();
    command.summary = "Record the project's initial intent".to_owned();
    command.initial_changes = vec![PlanChange::AddNode {
        node: PlanNodeDraft::new(
            node_id_for_request(&request_id, "initial-intent"),
            NodeKind::Intent,
            "Initial project intent",
            intent,
            NodeData::generic("active"),
            vec![provenance.clone()],
        ),
    }];
    if let Some(base_revision) = base_revision {
        command.initial_changes.push(PlanChange::AddNode {
            node: PlanNodeDraft::new(
                node_id_for_request(&request_id, "unverified-base-revision"),
                NodeKind::Assumption,
                "Unverified base revision",
                base_revision,
                NodeData::Assumption(AssumptionState {
                    status: AssumptionStatus::Unvalidated,
                    impact:
                        "Execution must not use this base until repository inspection verifies it"
                            .to_owned(),
                    validation_method: Some(
                        "Read the repository HEAD and resolve the supplied revision".to_owned(),
                    ),
                }),
                vec![provenance],
            ),
        });
    }
    Ok(command)
}

async fn list_projects(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let projects = state.agent.store.projects().list().await?;
    Ok(Json(json!({ "projects": projects })))
}

async fn inspect_project(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let project_id = parse_project_id(&project_id)?;
    let snapshot = state
        .agent
        .store
        .projects()
        .get(&project_id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("project {project_id}")))?;
    Ok(Json(snapshot_response(&state, &snapshot, None).await?))
}

async fn list_revisions(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let project_id = parse_project_id(&project_id)?;
    ensure_project_exists(&state, &project_id).await?;
    let revisions = state
        .agent
        .store
        .projects()
        .list_revisions(&project_id)
        .await?;
    Ok(Json(json!({
        "project_id": project_id,
        "revisions": revisions,
    })))
}

async fn apply_revision(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    Json(body): Json<ApplyRevisionRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let project_id = parse_project_id(&project_id)?;
    let request_id = body
        .request_id
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let stored_request = state
        .agent
        .store
        .projects()
        .get_revision_request(&project_id, &request_id)
        .await?;
    let created_at = stored_request
        .as_ref()
        .map(|request| request.created_at)
        .unwrap_or_else(Utc::now);
    let change_set = apply_revision_command(body, project_id, request_id, created_at);
    let expected_parent = change_set.parent_revision.clone();
    let result = state
        .agent
        .store
        .projects()
        .apply(&project_id, &expected_parent, change_set)
        .await?;
    let status = if result.replayed {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    let response = snapshot_response(&state, &result.snapshot, Some(result.replayed)).await?;
    Ok((status, Json(response)))
}

fn apply_revision_command(
    body: ApplyRevisionRequest,
    project_id: ProjectId,
    request_id: String,
    created_at: chrono::DateTime<Utc>,
) -> PlanChangeSet {
    let provenance = body
        .source_message
        .as_ref()
        .map(|source| {
            Provenance::conversation(ProvenanceClassification::UserStated, source, created_at)
        })
        .unwrap_or_else(|| Provenance {
            classification: ProvenanceClassification::UserStated,
            source: ProvenanceSource::Manual {
                reference: format!("authenticated-api-request:{request_id}"),
            },
            recorded_at: created_at,
            confidence: None,
            freshness: None,
        });

    let changes = body
        .changes
        .into_iter()
        .map(|change| match change {
            PlanChangeRequest::AddNode { node } => PlanChange::AddNode {
                node: node.into_domain(provenance.clone()),
            },
            PlanChangeRequest::UpdateNode { node_id, patch } => PlanChange::UpdateNode {
                node_id,
                patch,
                provenance: vec![provenance.clone()],
            },
            PlanChangeRequest::SupersedeNode {
                node_id,
                replacement,
            } => PlanChange::SupersedeNode {
                node_id,
                replacement: replacement.into_domain(provenance.clone()),
            },
            PlanChangeRequest::AddEdge { edge } => PlanChange::AddEdge {
                edge: edge.into_domain(provenance.clone()),
            },
            PlanChangeRequest::RemoveEdge { edge_id } => PlanChange::RemoveEdge {
                edge_id,
                provenance: vec![provenance.clone()],
            },
        })
        .collect();

    let mut command = PlanChangeSet::new(
        request_id,
        project_id,
        body.parent_revision,
        body.summary,
        RevisionAuthor::User,
        created_at,
        changes,
    );
    if let Some(source_message) = body.source_message {
        command = command.with_source_message(source_message);
    }
    if let Some(patch) = body.project_patch {
        command = command.with_project_patch(patch.into_domain(provenance));
    }
    command
}

impl PlanNodeRequest {
    fn into_domain(self, provenance: Provenance) -> PlanNodeDraft {
        PlanNodeDraft::new(
            self.id,
            self.kind,
            self.title,
            self.body,
            self.data,
            vec![provenance],
        )
    }
}

impl PlanEdgeRequest {
    fn into_domain(self, provenance: Provenance) -> PlanEdgeDraft {
        PlanEdgeDraft::new(self.id, self.from, self.relation, self.to, vec![provenance])
    }
}

impl ProjectPatchRequest {
    fn into_domain(self, provenance: Provenance) -> ProjectPatch {
        ProjectPatch {
            provenance: vec![provenance],
            title: self.title,
            status: self.status,
            repository_id: self.repository_id.map(Some),
            canonical_conversation_id: self.canonical_conversation_id.map(Some),
            judgment_policy: self.judgment_policy,
        }
    }
}

async fn inspect_revision(
    State(state): State<AppState>,
    Path((project_id, revision_id)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let project_id = parse_project_id(&project_id)?;
    let revision_id = parse_revision_id(&revision_id)?;
    let snapshot = state
        .agent
        .store
        .projects()
        .get_revision(&project_id, &revision_id)
        .await?
        .ok_or_else(|| {
            ApiError::not_found(format!("revision {revision_id} for project {project_id}"))
        })?;
    Ok(Json(snapshot_response(&state, &snapshot, None).await?))
}

async fn compare_revisions(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    Query(query): Query<CompareQuery>,
) -> Result<Json<Value>, ApiError> {
    let project_id = parse_project_id(&project_id)?;
    let from_id = parse_revision_id(&query.from)?;
    let to_id = parse_revision_id(&query.to)?;
    let from = load_revision(&state, &project_id, &from_id).await?;
    let to = load_revision(&state, &project_id, &to_id).await?;
    let comparison = from
        .revision
        .compare(&to.revision)
        .map_err(|error| ApiError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            code: "invalid_comparison",
            message: error.to_string(),
        })?;
    Ok(Json(json!({
        "project_id": project_id,
        "from_revision": from_id,
        "to_revision": to_id,
        "delta": comparison,
    })))
}

async fn project_projections(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    Query(query): Query<RevisionQuery>,
) -> Result<Json<Value>, ApiError> {
    let project_id = parse_project_id(&project_id)?;
    let snapshot = load_requested_snapshot(&state, &project_id, query.revision.as_deref()).await?;
    let mut value = serde_json::to_value(snapshot.revision.projections())
        .map_err(|error| ApiError::from(Error::Serialization(error)))?;
    normalize_projection_tree(&mut value, &snapshot.revision.id);
    align_projection_scope(&mut value, &snapshot.revision)?;
    Ok(Json(value))
}

async fn project_projection(
    State(state): State<AppState>,
    Path((project_id, kind)): Path<(String, String)>,
    Query(query): Query<RevisionQuery>,
) -> Result<Json<Value>, ApiError> {
    let project_id = parse_project_id(&project_id)?;
    let snapshot = load_requested_snapshot(&state, &project_id, query.revision.as_deref()).await?;
    let projection = snapshot.revision.projection(parse_projection_kind(&kind)?);
    let mut value = serde_json::to_value(projection)
        .map_err(|error| ApiError::from(Error::Serialization(error)))?;
    if let Some(inner) = value
        .as_object_mut()
        .and_then(|object| object.remove("projection"))
    {
        value = inner;
    }
    normalize_projection_tree(&mut value, &snapshot.revision.id);
    align_projection_scope(&mut value, &snapshot.revision)?;
    Ok(Json(value))
}

async fn ensure_project_exists(state: &AppState, project_id: &ProjectId) -> Result<(), ApiError> {
    if state
        .agent
        .store
        .projects()
        .get(project_id)
        .await?
        .is_some()
    {
        Ok(())
    } else {
        Err(ApiError::not_found(format!("project {project_id}")))
    }
}

async fn load_requested_snapshot(
    state: &AppState,
    project_id: &ProjectId,
    revision: Option<&str>,
) -> Result<ProjectSnapshot, ApiError> {
    match revision {
        Some(revision) => {
            let revision_id = parse_revision_id(revision)?;
            load_revision(state, project_id, &revision_id).await
        }
        None => state
            .agent
            .store
            .projects()
            .get(project_id)
            .await?
            .ok_or_else(|| ApiError::not_found(format!("project {project_id}"))),
    }
}

async fn load_revision(
    state: &AppState,
    project_id: &ProjectId,
    revision_id: &RevisionId,
) -> Result<ProjectSnapshot, ApiError> {
    state
        .agent
        .store
        .projects()
        .get_revision(project_id, revision_id)
        .await?
        .ok_or_else(|| {
            ApiError::not_found(format!("revision {revision_id} for project {project_id}"))
        })
}

fn parse_project_id(value: &str) -> Result<ProjectId, ApiError> {
    Uuid::parse_str(value)
        .map(ProjectId::from_uuid)
        .map_err(|_| ApiError::bad_request(format!("invalid project id: {value}")))
}

fn parse_revision_id(value: &str) -> Result<RevisionId, ApiError> {
    serde_json::from_value(Value::String(value.to_owned()))
        .map_err(|_| ApiError::bad_request(format!("invalid revision id: {value}")))
}

fn parse_projection_kind(value: &str) -> Result<ProjectionKind, ApiError> {
    match value {
        "brief" => Ok(ProjectionKind::Brief),
        "roadmap" => Ok(ProjectionKind::Roadmap),
        "decision" | "decisions" | "decision-log" | "decision_log" => Ok(ProjectionKind::Decisions),
        "question" | "questions" => Ok(ProjectionKind::Questions),
        "risk" | "risks" => Ok(ProjectionKind::Risks),
        "architecture" => Ok(ProjectionKind::Architecture),
        "behavior" | "behaviors" | "behavior-catalog" | "behavior_catalog" => {
            Ok(ProjectionKind::BehaviorCatalog)
        }
        "current" | "current-understanding" | "current_understanding" => {
            Ok(ProjectionKind::CurrentUnderstanding)
        }
        _ => Err(ApiError::bad_request(format!(
            "unknown projection: {value}"
        ))),
    }
}

fn source_message(
    conversation_id: Option<&str>,
    message_id: Option<&str>,
) -> Result<Option<MessageRef>, ApiError> {
    match (conversation_id, message_id) {
        (None, None) => Ok(None),
        (Some(conversation_id), Some(message_id)) => MessageRef::new(conversation_id, message_id)
            .map(Some)
            .map_err(|error| ApiError::bad_request(error.to_string())),
        _ => Err(ApiError::bad_request(
            "canonical_conversation_id and source_message_id must be supplied together",
        )),
    }
}

fn title_from_intent(intent: &str) -> String {
    const MAX_CHARS: usize = 80;
    let first_line = intent.lines().next().unwrap_or(intent).trim();
    let mut title: String = first_line.chars().take(MAX_CHARS).collect();
    if first_line.chars().count() > MAX_CHARS {
        title.push('\u{2026}');
    }
    title
}

fn project_id_for_request(request_id: &str) -> ProjectId {
    ProjectId::from_uuid(uuid_for_request(request_id, "project"))
}

fn node_id_for_request(request_id: &str, role: &str) -> NodeId {
    NodeId::from_uuid(uuid_for_request(request_id, role))
}

fn uuid_for_request(request_id: &str, role: &str) -> Uuid {
    let digest = Sha256::digest(format!("rustykrab-project-request:{role}:{request_id}"));
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    // Encode RFC 9562 version/variant bits so the derived identifier behaves
    // like a regular UUID while remaining stable for idempotent retries.
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

async fn snapshot_response(
    state: &AppState,
    snapshot: &ProjectSnapshot,
    replayed: Option<bool>,
) -> Result<Value, ApiError> {
    let revisions = state
        .agent
        .store
        .projects()
        .list_revisions(&snapshot.project.id)
        .await?;
    Ok(snapshot_json(snapshot, &revisions, replayed))
}

fn snapshot_json(
    snapshot: &ProjectSnapshot,
    revisions: &[rustykrab_projects::ProjectRevision],
    replayed: Option<bool>,
) -> Value {
    let revision_count = snapshot.revision.sequence.saturating_add(1);
    let mut value = json!({
        "id": snapshot.project.id,
        "title": snapshot.project.title,
        "status": snapshot.project.status,
        "repository_id": snapshot.project.repository_id,
        "canonical_conversation_id": snapshot.project.canonical_conversation_id,
        "current_revision": snapshot.revision.id,
        "revision_count": revision_count,
        "project": snapshot.project,
        "revision": snapshot.revision,
        "conversation_links": conversation_links(snapshot, revisions),
    });
    if let Some(replayed) = replayed {
        value["replayed"] = Value::Bool(replayed);
    }
    value
}

fn conversation_links(
    snapshot: &ProjectSnapshot,
    revisions: &[rustykrab_projects::ProjectRevision],
) -> Vec<Value> {
    let mut links: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for revision in revisions
        .iter()
        .filter(|revision| revision.sequence <= snapshot.revision.sequence)
    {
        if let Some(source) = &revision.source_message {
            links
                .entry(source.conversation_id.clone())
                .or_default()
                .insert(source.message_id.clone());
        }
        for provenance in &revision.project_provenance {
            record_conversation_provenance(&mut links, provenance);
        }
    }
    for provenance in snapshot
        .revision
        .nodes
        .values()
        .flat_map(|node| node.provenance.iter())
        .chain(
            snapshot
                .revision
                .edges
                .values()
                .flat_map(|edge| edge.provenance.iter()),
        )
    {
        record_conversation_provenance(&mut links, provenance);
    }
    links
        .into_iter()
        .map(|(conversation_id, source_message_ids)| {
            json!({
                "conversation_id": conversation_id,
                "source_message_ids": source_message_ids,
            })
        })
        .collect()
}

fn record_conversation_provenance(
    links: &mut BTreeMap<String, BTreeSet<String>>,
    provenance: &Provenance,
) {
    if let ProvenanceSource::ConversationMessage {
        conversation_id,
        message_id,
    } = &provenance.source
    {
        links
            .entry(conversation_id.clone())
            .or_default()
            .insert(message_id.clone());
    }
}

fn normalize_projection_tree(value: &mut Value, revision_id: &RevisionId) {
    if let Some(object) = value.as_object_mut() {
        let had_revision = if let Some(id) = object.remove("revision_id") {
            object.insert("source_revision".to_owned(), id);
            true
        } else {
            false
        };
        let is_projection_set = object.contains_key("brief") && object.contains_key("roadmap");
        if had_revision || is_projection_set {
            object
                .entry("source_revision")
                .or_insert_with(|| Value::String(revision_id.to_string()));
        }

        if let Some(brief) = object.get_mut("brief") {
            augment_brief(brief);
        }
        for child in object.values_mut() {
            if child.is_object() {
                normalize_projection_tree(child, revision_id);
            }
        }
        augment_brief(value);
    }
}

fn augment_brief(value: &mut Value) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    if object.contains_key("intents") {
        let intent = object
            .get("intents")
            .and_then(Value::as_array)
            .and_then(|intents| intents.first())
            .and_then(|intent| intent.get("body"))
            .cloned()
            .unwrap_or(Value::Null);
        object.entry("intent").or_insert(intent);
        let scope = object
            .get("requirements")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new()));
        object.entry("scope").or_insert(scope);
    }
}

fn align_projection_scope(
    value: &mut Value,
    revision: &rustykrab_projects::ProjectRevision,
) -> Result<(), ApiError> {
    let scope = serde_json::to_value(revision.brief_projection().requirements)
        .map_err(|error| ApiError::from(Error::Serialization(error)))?;
    align_projection_scope_value(value, &scope);
    Ok(())
}

fn align_projection_scope_value(value: &mut Value, scope: &Value) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    for name in ["brief", "roadmap"] {
        if let Some(projection) = object.get_mut(name).and_then(Value::as_object_mut) {
            projection.insert("scope".to_owned(), scope.clone());
        }
    }
    if object.contains_key("intents") || object.contains_key("workstreams") {
        object.insert("scope".to_owned(), scope.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_request(base_revision: Option<&str>) -> CreateProjectRequest {
        CreateProjectRequest {
            request_id: Some("create-request-1".to_owned()),
            project_id: None,
            title: None,
            repository_id: Some("gcbh/rustykrab".to_owned()),
            repository_path: None,
            base_revision: base_revision.map(str::to_owned),
            canonical_conversation_id: Some("conversation-1".to_owned()),
            source_message_id: Some("message-1".to_owned()),
            message: Some("Build a durable project planner".to_owned()),
            intent: None,
            judgment_policy: None,
        }
    }

    #[test]
    fn title_is_unicode_safe_and_bounded() {
        let title = title_from_intent(&"🦀".repeat(100));
        assert_eq!(title.chars().count(), 81);
        assert!(title.ends_with('\u{2026}'));
    }

    #[test]
    fn source_message_requires_both_identifiers() {
        assert!(source_message(Some("conversation"), None).is_err());
        assert!(source_message(None, Some("message")).is_err());
        assert!(source_message(Some("conversation"), Some("message"))
            .unwrap()
            .is_some());
    }

    #[test]
    fn request_id_derives_a_stable_project_id() {
        assert_eq!(
            project_id_for_request("request-1"),
            project_id_for_request("request-1")
        );
        assert_ne!(
            project_id_for_request("request-1"),
            project_id_for_request("request-2")
        );
    }

    #[test]
    fn retry_command_is_exact_when_the_stored_server_time_is_reused() {
        let created_at = chrono::DateTime::parse_from_rfc3339("2026-09-01T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let first = create_project_command(
            create_request(Some("0123456789abcdef")),
            "create-request-1".to_owned(),
            created_at,
        )
        .unwrap();
        let retry = create_project_command(
            create_request(Some("0123456789abcdef")),
            "create-request-1".to_owned(),
            first.created_at,
        )
        .unwrap();

        assert_eq!(retry, first);
        assert_eq!(first.initial_changes.len(), 2);
        let ids: Vec<_> = first
            .initial_changes
            .iter()
            .filter_map(|change| match change {
                PlanChange::AddNode { node } => Some(node.id),
                _ => None,
            })
            .collect();
        assert_eq!(
            ids[0],
            node_id_for_request("create-request-1", "initial-intent")
        );
        assert_eq!(
            ids[1],
            node_id_for_request("create-request-1", "unverified-base-revision")
        );
    }

    #[test]
    fn changing_the_unverified_base_changes_the_create_command() {
        let created_at = Utc::now();
        let first = create_project_command(
            create_request(Some("base-a")),
            "create-request-1".to_owned(),
            created_at,
        )
        .unwrap();
        let changed = create_project_command(
            create_request(Some("base-b")),
            "create-request-1".to_owned(),
            created_at,
        )
        .unwrap();

        assert_ne!(changed, first);
        let PlanChange::AddNode { node } = &first.initial_changes[1] else {
            panic!("base revision should be recorded as a node");
        };
        assert_eq!(node.kind, NodeKind::Assumption);
        assert!(matches!(
            node.data,
            NodeData::Assumption(AssumptionState {
                status: AssumptionStatus::Unvalidated,
                ..
            })
        ));
    }

    #[test]
    fn revision_request_rejects_client_author_time_and_provenance() {
        let error = serde_json::from_value::<ApplyRevisionRequest>(json!({
            "request_id": "revision-request-1",
            "parent_revision": "a".repeat(64),
            "summary": "Attempt to forge trusted fields",
            "author": "system",
            "created_at": "2000-01-01T00:00:00Z",
            "provenance": [{
                "classification": "repository_observed",
                "source": {"type": "repository", "repository": "other/repo"}
            }],
            "changes": []
        }))
        .unwrap_err();

        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn revision_command_derives_actor_time_and_provenance() {
        let created_at = chrono::DateTime::parse_from_rfc3339("2026-09-04T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let node_id = NodeId::new();
        let body: ApplyRevisionRequest = serde_json::from_value(json!({
            "request_id": "revision-request-1",
            "parent_revision": "a".repeat(64),
            "summary": "Add a verified outcome",
            "source_message": {
                "conversation_id": "conversation-1",
                "message_id": "message-2"
            },
            "changes": [{
                "operation": "add_node",
                "node": {
                    "id": node_id,
                    "kind": "outcome",
                    "title": "Durable planning works",
                    "body": "The service preserves revisions across restart.",
                    "data": {
                        "type": "outcome",
                        "state": {
                            "status": "achieved",
                            "success_measures": ["Distinct process reopened the same revision"]
                        }
                    }
                }
            }]
        }))
        .unwrap();
        let project_id = ProjectId::new();
        let first = apply_revision_command(
            body.clone(),
            project_id,
            "revision-request-1".to_owned(),
            created_at,
        );
        let retry = apply_revision_command(
            body,
            project_id,
            "revision-request-1".to_owned(),
            first.created_at,
        );

        assert_eq!(first, retry);
        assert_eq!(first.author, RevisionAuthor::User);
        assert_eq!(first.created_at, created_at);
        assert_eq!(
            first.source_message,
            Some(MessageRef::new("conversation-1", "message-2").unwrap())
        );
        let PlanChange::AddNode { node } = &first.changes[0] else {
            panic!("request should produce an add-node change");
        };
        assert_eq!(node.provenance.len(), 1);
        assert_eq!(
            node.provenance[0].classification,
            ProvenanceClassification::UserStated
        );
        assert_eq!(node.provenance[0].recorded_at, created_at);
        assert!(matches!(
            &node.provenance[0].source,
            ProvenanceSource::ConversationMessage {
                conversation_id,
                message_id,
            } if conversation_id == "conversation-1" && message_id == "message-2"
        ));
    }

    #[test]
    fn combined_and_standalone_roadmaps_receive_the_same_scope() {
        let scope = json!([{"id": "requirement-1", "title": "Durable planning"}]);
        let mut combined = json!({
            "brief": {"intents": [], "requirements": scope.clone(), "scope": []},
            "roadmap": {"workstreams": [{"title": "Implementation"}]}
        });
        let mut standalone = json!({
            "workstreams": [{"title": "Implementation"}],
            "scope": [{"title": "Implementation"}]
        });

        align_projection_scope_value(&mut combined, &scope);
        align_projection_scope_value(&mut standalone, &scope);

        assert_eq!(combined["brief"]["scope"], scope);
        assert_eq!(combined["roadmap"]["scope"], scope);
        assert_eq!(standalone["scope"], scope);
    }
}
