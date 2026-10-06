//! Questions and standing judgment: the question
//! router's and standing judgment's HTTP surface
//! (`docs/plans/control-layer-and-worker-fleet.md`, sections 5, 7 and 14).
//!
//! Reads of questions come straight from the store; answering and granting
//! go through the controller's [`ControlHandle`], so an answer resumes the
//! item that asked (not a conversation) in the same transaction that
//! settles the question. Each command carries the authenticated principal
//! as its actor.
//!
//! - `GET /api/questions` lists questions oldest first (`item`, `root`,
//!   `status`, `waiting=true` filter); `GET /api/questions/{id}` reads one.
//! - `POST /api/questions/{id}/answer` with `{ "answer": "..." }` answers
//!   one, by its id or the first eight characters of it. 409 when it is
//!   already settled, 400 when the answer does not fit it (a consent takes
//!   yes or no; a plan's approval takes approve or reject).
//! - `GET /api/judgment` shows the grants in force and the rules they add up
//!   to; `POST /api/judgment` with `{ "text": "...", "scope": "..." }`
//!   grants standing judgment in ordinary language and answers with what it
//!   compiled to, including any sentence it did not understand;
//!   `POST /api/judgment/{id}/revoke` revokes one.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use rustykrab_control::handle::{AnswerReply, ControlHandle, JudgmentView};
use rustykrab_core::questions::QuestionStatus;
use rustykrab_core::{Error, ToolErrorKind};
use rustykrab_store::{JudgmentRow, Principal, QuestionFilter, QuestionRow};

use crate::AppState;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/questions", get(list_questions))
        .route("/api/questions/{id}", get(get_question))
        .route("/api/questions/{id}/answer", post(answer))
        .route("/api/judgment", get(judgment).post(grant))
        .route("/api/judgment/{id}/revoke", post(revoke))
}

/// `GET /api/questions`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuestionList {
    pub questions: Vec<QuestionRow>,
}

/// `POST /api/judgment/{id}/revoke`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RevokeReply {
    pub id: String,
    pub revoked: bool,
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        ApiError {
            status,
            code,
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request", message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({ "error": self.code, "message": self.message })),
        )
            .into_response()
    }
}

impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        match error {
            Error::NotFound(m) => Self::new(StatusCode::NOT_FOUND, "not_found", m),
            Error::AlreadyExists(m) => Self::new(StatusCode::CONFLICT, "already_settled", m),
            Error::ToolExecution(te) if te.kind == ToolErrorKind::InvalidInput => {
                Self::bad_request(te.message)
            }
            Error::Auth(m) => Self::new(StatusCode::FORBIDDEN, "forbidden", m),
            Error::Internal(m) => Self::new(StatusCode::CONFLICT, "conflict", m),
            other => {
                tracing::error!(error = %other, "question API operation failed");
                Self::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    "question operation failed",
                )
            }
        }
    }
}

impl From<rustykrab_store::WorkStoreError> for ApiError {
    fn from(error: rustykrab_store::WorkStoreError) -> Self {
        Error::from(error).into()
    }
}

fn control(state: &AppState) -> Result<Arc<dyn ControlHandle>, ApiError> {
    state.control.clone().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "control_unavailable",
            "the control layer is not running in this daemon",
        )
    })
}

fn actor_of(principal: Option<Extension<Principal>>) -> String {
    match principal {
        Some(Extension(p)) => format!("user:{}", p.describe()),
        None => "user".to_string(),
    }
}

fn body_json<T: for<'de> Deserialize<'de>>(body: &Bytes, what: &str) -> Result<T, ApiError> {
    serde_json::from_slice(body).map_err(|e| ApiError::bad_request(format!("invalid {what}: {e}")))
}

#[derive(Debug, Default, Deserialize)]
struct ListQuery {
    item: Option<String>,
    root: Option<String>,
    status: Option<String>,
    waiting: Option<String>,
}

/// `GET /api/questions`: oldest first.
async fn list_questions(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> Result<Json<QuestionList>, ApiError> {
    let status = match query.status.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(raw) => {
            let parsed = QuestionStatus::ALL
                .iter()
                .copied()
                .find(|s| s.as_str() == raw)
                .ok_or_else(|| ApiError::bad_request(format!("unknown status `{raw}`")))?;
            Some(parsed)
        }
    };
    let waiting = match query.waiting.as_deref().map(str::trim) {
        None | Some("") | Some("false") | Some("0") => false,
        Some("true") | Some("1") => true,
        Some(other) => {
            return Err(ApiError::bad_request(format!(
                "`waiting` must be true or false, got `{other}`"
            )))
        }
    };
    let filter = QuestionFilter {
        item: query.item.filter(|s| !s.trim().is_empty()),
        root: query.root.filter(|s| !s.trim().is_empty()),
        status,
        waiting,
    };
    let mut questions = state.agent.store.questions_list(&filter).await?;
    questions.reverse();
    Ok(Json(QuestionList { questions }))
}

/// `GET /api/questions/{id}`.
async fn get_question(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<QuestionRow>, ApiError> {
    state
        .agent
        .store
        .question_get(&id)
        .await?
        .map(Json)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "not_found",
                format!("no question {id}"),
            )
        })
}

#[derive(Debug, Deserialize)]
struct AnswerBody {
    answer: String,
}

/// `POST /api/questions/{id}/answer`.
async fn answer(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Json<AnswerReply>, ApiError> {
    let control = control(&state)?;
    let parsed: AnswerBody = body_json(&body, "answer")?;
    if parsed.answer.trim().is_empty() {
        return Err(ApiError::bad_request("an answer needs words"));
    }
    let reply = control
        .answer(&id, &parsed.answer, &actor_of(principal))
        .await?;
    Ok(Json(reply))
}

/// `GET /api/judgment`.
async fn judgment(State(state): State<AppState>) -> Result<Json<JudgmentView>, ApiError> {
    Ok(Json(control(&state)?.judgment().await?))
}

#[derive(Debug, Deserialize)]
struct GrantBody {
    text: String,
    #[serde(default)]
    scope: Option<String>,
}

/// `POST /api/judgment`: 201 with what the words compiled to.
async fn grant(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let control = control(&state)?;
    let parsed: GrantBody = body_json(&body, "grant")?;
    let row: JudgmentRow = control
        .grant_judgment(
            &parsed.text,
            parsed.scope.as_deref().unwrap_or(""),
            &actor_of(principal),
        )
        .await?;
    Ok((StatusCode::CREATED, Json(row)).into_response())
}

/// `POST /api/judgment/{id}/revoke`.
async fn revoke(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
    Path(id): Path<String>,
) -> Result<Json<RevokeReply>, ApiError> {
    let revoked = control(&state)?
        .revoke_judgment(&id, &actor_of(principal))
        .await?;
    Ok(Json(RevokeReply { id, revoked }))
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use async_trait::async_trait;
    use chrono::Utc;
    use reqwest::header::{HeaderMap, HeaderValue, ORIGIN};
    use reqwest::StatusCode as Http;
    use rustykrab_control::controller::{Controller, ControllerConfig};
    use rustykrab_control::Provenance;
    use rustykrab_core::model::{ModelProvider, ModelResponse};
    use rustykrab_core::questions::{QuestionClass, QuestionKind};
    use rustykrab_core::types::{Message, ToolSchema};
    use rustykrab_core::work::{BlockedReason, PlanOutcome, Status, WorkItemDraft};
    use rustykrab_store::{QuestionWrite, Store};
    use serde_json::{json, Value};
    use uuid::Uuid;

    use super::*;

    const TOKEN: &str = "question-routes-test-token";

    struct UnusedProvider;

    #[async_trait]
    impl ModelProvider for UnusedProvider {
        fn name(&self) -> &str {
            "unused"
        }
        async fn chat(
            &self,
            _: &[Message],
            _: &[ToolSchema],
        ) -> rustykrab_core::Result<ModelResponse> {
            Err(Error::ModelProvider("not used by these tests".into()))
        }
    }

    struct Harness {
        base: String,
        client: reqwest::Client,
        store: Store,
        control: Arc<Controller>,
    }

    async fn harness() -> Harness {
        let dir = std::env::temp_dir().join(format!("rk-question-routes-{}", Uuid::new_v4()));
        let store = Store::open(&dir, vec![9u8; 32]).expect("store opens");
        let control = Arc::new(Controller::new(
            store.clone(),
            vec![],
            ControllerConfig::default(),
        ));
        let state = AppState::new(
            store.clone(),
            vec![],
            Arc::new(UnusedProvider),
            TOKEN.into(),
        )
        .with_control(control.clone())
        .with_workers(control.registry().clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = crate::router(state);
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        let base = format!("http://{addr}");
        let mut headers = HeaderMap::new();
        headers.insert(ORIGIN, HeaderValue::from_str(&base).unwrap());
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .unwrap();
        Harness {
            base,
            client,
            store,
            control,
        }
    }

    impl Harness {
        async fn get(&self, path: &str) -> (Http, Value) {
            let r = self
                .client
                .get(format!("{}{path}", self.base))
                .bearer_auth(TOKEN)
                .send()
                .await
                .unwrap();
            (r.status(), r.json().await.unwrap_or(Value::Null))
        }

        async fn post(&self, path: &str, body: Value) -> (Http, Value) {
            let r = self
                .client
                .post(format!("{}{path}", self.base))
                .bearer_auth(TOKEN)
                .json(&body)
                .send()
                .await
                .unwrap();
            (r.status(), r.json().await.unwrap_or(Value::Null))
        }

        /// An item parked on an open question, as the router leaves one.
        async fn parked(&self) -> (String, String) {
            let outcome = self
                .control
                .file_draft(
                    WorkItemDraft {
                        title: "Order flowers".into(),
                        objective: "o".into(),
                        done_when: "d".into(),
                        ..WorkItemDraft::default()
                    },
                    Provenance::default(),
                )
                .await
                .unwrap();
            let PlanOutcome::Accepted(a) = outcome else {
                panic!("rejected");
            };
            let item = a.root;
            let from = self.store.work_get(&item).await.unwrap().unwrap().status;
            self.store
                .work_transition(
                    &item,
                    Some(from),
                    Status::Blocked(BlockedReason::NeedsDecision),
                    "controller",
                    None,
                    None,
                    None,
                    None,
                )
                .await
                .unwrap();
            let question = Uuid::new_v4().to_string();
            self.store
                .question_write(QuestionWrite::Insert(Box::new(QuestionRow {
                    id: question.clone(),
                    item: item.clone(),
                    root: item.clone(),
                    kind: QuestionKind::Decision,
                    class: QuestionClass::BlockingNow,
                    text: "Which florist?".into(),
                    options: vec!["Petals".into(), "Stems".into()],
                    default_answer: None,
                    asked_class: Some("blocking_now".into()),
                    rule: "blocking_now".into(),
                    asked_by: "worker:pinch".into(),
                    status: QuestionStatus::Open,
                    delivered_via: Some("telegram".into()),
                    answer: None,
                    answered_by: None,
                    answered_at: None,
                    research_item: None,
                    decision: None,
                    created_at: Utc::now(),
                })))
                .await
                .unwrap();
            (item, question)
        }
    }

    #[tokio::test]
    async fn a_question_is_listed_answered_once_and_resumes_its_item() {
        let h = harness().await;
        let (item, question) = h.parked().await;
        let (status, body) = h.get(&format!("/api/questions?item={item}")).await;
        assert_eq!(status, Http::OK, "{body}");
        assert_eq!(body["questions"][0]["id"], question.as_str());
        assert_eq!(body["questions"][0]["class"], "blocking_now");
        let (status, _) = h.get("/api/questions?status=bogus").await;
        assert_eq!(status, Http::BAD_REQUEST);

        let path = format!("/api/questions/{question}/answer");
        let (status, body) = h.post(&path, json!({ "answer": "1" })).await;
        assert_eq!(status, Http::OK, "{body}");
        assert_eq!(body["question"]["answer"], "Petals");
        assert_eq!(body["resumed"][0], item.as_str());
        let resumed = h.store.work_get(&item).await.unwrap().unwrap().status;
        assert!(
            resumed == Status::Queued || resumed == Status::Ready,
            "{resumed}"
        );
        let (status, _) = h.post(&path, json!({ "answer": "2" })).await;
        assert_eq!(status, Http::CONFLICT, "answered once only");
        let (status, _) = h.post(&path, json!({ "answer": "  " })).await;
        assert_eq!(status, Http::BAD_REQUEST);
        let (_, waiting) = h.get("/api/questions?waiting=true").await;
        assert_eq!(waiting["questions"].as_array().unwrap().len(), 0);
        let (status, _) = h
            .post("/api/questions/nope/answer", json!({ "answer": "x" }))
            .await;
        assert_eq!(status, Http::NOT_FOUND);
    }

    #[tokio::test]
    async fn standing_judgment_is_granted_read_back_and_revoked() {
        let h = harness().await;
        let (status, row) = h
            .post(
                "/api/judgment",
                json!({ "text": "Ask me before paying for anything. Make it snappy." }),
            )
            .await;
        assert_eq!(status, Http::CREATED, "{row}");
        assert_eq!(row["checks"][0]["check"], "consent_for");
        assert_eq!(row["unrecognised"][0], "Make it snappy");
        let (status, view) = h.get("/api/judgment").await;
        assert_eq!(status, Http::OK);
        assert_eq!(view["grants"].as_array().unwrap().len(), 1);
        assert!(view["rules"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r.as_str().unwrap().contains("payment")));
        let id = row["id"].as_str().unwrap();
        let (status, revoked) = h
            .post(&format!("/api/judgment/{id}/revoke"), json!({}))
            .await;
        assert_eq!(status, Http::OK);
        assert_eq!(revoked["revoked"], true);
        let (_, workers) = h.get("/api/workers").await;
        assert_eq!(workers["workers"], json!([]));
    }
}
