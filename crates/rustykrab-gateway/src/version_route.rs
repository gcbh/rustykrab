//! `GET /api/version`: which build is running and whether its controller is
//! alive, for an updater verifying a cutover. `GET /api/health` stays the
//! bare `ok` install scripts read; this route sits behind the same auth and
//! origin rules as the rest of `/api`.

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::AppState;

pub(crate) fn routes() -> Router<AppState> {
    Router::new().route("/api/version", get(version))
}

#[derive(Debug, Serialize)]
struct VersionReply {
    version: String,
    commit: Option<String>,
    build_date: Option<String>,
    controller: ControllerReply,
}

/// The controller's state. `last_tick` and `runs_in_flight` are `None`
/// when no controller is wired, or when the wired handle runs no loop.
#[derive(Debug, Serialize)]
struct ControllerReply {
    wired: bool,
    last_tick: Option<DateTime<Utc>>,
    runs_in_flight: Option<usize>,
}

async fn version(State(state): State<AppState>) -> Json<VersionReply> {
    let status = state.control.as_ref().and_then(|c| c.loop_status());
    Json(VersionReply {
        version: state.build.version.clone(),
        commit: state.build.commit.clone(),
        build_date: state.build.build_date.clone(),
        controller: ControllerReply {
            wired: state.control.is_some(),
            last_tick: status.as_ref().and_then(|s| s.last_tick),
            runs_in_flight: status.map(|s| s.runs_in_flight),
        },
    })
}

#[cfg(test)]
mod tests {
    //! A real router on a loopback port, behind the real auth, origin and
    //! rate-limit middleware.

    use std::net::SocketAddr;
    use std::sync::Arc;

    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use reqwest::header::ORIGIN;
    use reqwest::StatusCode as Http;
    use serde_json::Value;
    use uuid::Uuid;

    use rustykrab_control::graph::FilingSource;
    use rustykrab_control::handle::{ControlHandle, GraphView, LoopStatus, TickReport};
    use rustykrab_control::Provenance;
    use rustykrab_core::model::{ModelProvider, ModelResponse};
    use rustykrab_core::types::{Message, ToolSchema};
    use rustykrab_core::work::{PlanOutcome, WorkItemId, WorkPlan};
    use rustykrab_core::Error;
    use rustykrab_store::Store;

    use crate::{AppState, BuildInfo};

    const TOKEN: &str = "version-route-test-token";

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

    /// A controller that only reports its loop.
    struct StubControl(Option<LoopStatus>);

    fn unused() -> Error {
        Error::Internal("not used by these tests".into())
    }

    #[async_trait]
    impl ControlHandle for StubControl {
        async fn file_plan(
            &self,
            _: WorkPlan,
            _: Provenance,
            _: FilingSource,
        ) -> Result<PlanOutcome, Error> {
            Err(unused())
        }

        async fn approve(&self, _: &str, _: &str) -> Result<Vec<WorkItemId>, Error> {
            Err(unused())
        }

        async fn reject(
            &self,
            _: &str,
            _: Option<String>,
            _: &str,
        ) -> Result<Vec<WorkItemId>, Error> {
            Err(unused())
        }

        async fn cancel(
            &self,
            _: &str,
            _: Option<String>,
            _: &str,
        ) -> Result<Vec<WorkItemId>, Error> {
            Err(unused())
        }

        async fn tick(&self) -> Result<TickReport, Error> {
            Err(unused())
        }

        async fn graph(&self, _: &str) -> Result<GraphView, Error> {
            Err(unused())
        }

        fn loop_status(&self) -> Option<LoopStatus> {
            self.0.clone()
        }
    }

    fn at() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-28T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    /// Serve `state` and return its base URL.
    async fn serve(state: AppState) -> String {
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
        format!("http://{addr}")
    }

    fn state() -> AppState {
        let dir = std::env::temp_dir().join(format!("rk-version-route-{}", Uuid::new_v4()));
        let store = Store::open(&dir, vec![9u8; 32]).expect("store opens");
        AppState::new(store, vec![], Arc::new(UnusedProvider), TOKEN.into())
    }

    async fn get_version(base: &str) -> (Http, Value) {
        let response = reqwest::Client::new()
            .get(format!("{base}/api/version"))
            .bearer_auth(TOKEN)
            .header(ORIGIN, base)
            .send()
            .await
            .unwrap();
        let status = response.status();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn version_reports_the_build_and_the_controller_loop() {
        let base = serve(
            state()
                .with_build_info(BuildInfo {
                    version: "1.2.3".into(),
                    commit: Some("abc1234-dirty".into()),
                    build_date: Some("2026-09-27".into()),
                })
                .with_control(Arc::new(StubControl(Some(LoopStatus {
                    last_tick: Some(at()),
                    runs_in_flight: 2,
                })))),
        )
        .await;
        let (status, body) = get_version(&base).await;
        assert_eq!(status, Http::OK, "{body}");
        assert_eq!(body["version"], "1.2.3");
        assert_eq!(body["commit"], "abc1234-dirty");
        assert_eq!(body["build_date"], "2026-09-27");
        assert_eq!(body["controller"]["wired"], true);
        assert_eq!(body["controller"]["runs_in_flight"], 2);
        let last: DateTime<Utc> = body["controller"]["last_tick"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(last, at());
    }

    #[tokio::test]
    async fn version_without_a_controller_says_so() {
        let base = serve(state()).await;
        let (status, body) = get_version(&base).await;
        assert_eq!(status, Http::OK, "{body}");
        assert_eq!(body["version"], rustykrab_core::VERSION);
        assert_eq!(body["commit"], Value::Null);
        assert_eq!(body["controller"]["wired"], false);
        assert_eq!(body["controller"]["last_tick"], Value::Null);
        assert_eq!(body["controller"]["runs_in_flight"], Value::Null);
    }

    #[tokio::test]
    async fn a_controller_before_its_first_tick_reports_no_tick() {
        let base =
            serve(state().with_control(Arc::new(StubControl(Some(LoopStatus::default()))))).await;
        let (_, body) = get_version(&base).await;
        assert_eq!(body["controller"]["wired"], true);
        assert_eq!(body["controller"]["last_tick"], Value::Null);
        assert_eq!(body["controller"]["runs_in_flight"], 0);
    }

    #[tokio::test]
    async fn version_needs_auth_and_a_trusted_origin_while_health_stays_ok() {
        let base = serve(state()).await;
        let client = reqwest::Client::new();

        let anonymous = client
            .get(format!("{base}/api/version"))
            .header(ORIGIN, &base)
            .send()
            .await
            .unwrap();
        assert_eq!(anonymous.status(), Http::UNAUTHORIZED);

        let foreign = client
            .get(format!("{base}/api/version"))
            .bearer_auth(TOKEN)
            .header(ORIGIN, "https://evil.example")
            .send()
            .await
            .unwrap();
        assert_eq!(foreign.status(), Http::FORBIDDEN);

        let health = client
            .get(format!("{base}/api/health"))
            .header(ORIGIN, &base)
            .send()
            .await
            .unwrap();
        assert_eq!(health.status(), Http::OK);
        assert_eq!(health.text().await.unwrap(), "ok");
    }
}
