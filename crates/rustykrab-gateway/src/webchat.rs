use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use rust_embed::Embed;

use crate::AppState;

#[derive(Embed)]
#[folder = "static/"]
struct StaticAssets;

pub fn static_routes() -> Router<AppState> {
    Router::new().route("/", get(index)).route("/{*path}", get(serve_static))
}

async fn index() -> impl IntoResponse {
    serve_file("index.html")
}

async fn serve_static(axum::extract::Path(path): axum::extract::Path<String>) -> impl IntoResponse {
    serve_file(&path)
}

fn serve_file(path: &str) -> impl IntoResponse {
    match StaticAssets::get(path) {
        Some(file) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, mime.as_ref().to_string())],
                file.data.to_vec(),
            )
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
