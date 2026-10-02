use std::borrow::Cow;

use axum::body::Body;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use rust_embed::Embed;

use crate::AppState;

#[derive(Embed)]
#[folder = "static/"]
struct StaticAssets;

pub fn static_routes() -> Router<AppState> {
    Router::new()
        .route("/", get(index))
        .route("/{*path}", get(serve_static))
}

async fn index(headers: HeaderMap) -> impl IntoResponse {
    serve_file("index.html", &headers)
}

async fn serve_static(
    axum::extract::Path(path): axum::extract::Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    serve_file(&path, &headers)
}

/// Assets are embedded in the binary, so a cached copy is valid until the
/// binary changes; `no-cache` forces revalidation (via `If-None-Match`)
/// without re-downloading unchanged bodies.
const CACHE_CONTROL_VALUE: &str = "no-cache";

fn serve_file(path: &str, request_headers: &HeaderMap) -> Response {
    let Some(file) = StaticAssets::get(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let etag = format!("\"{}\"", hex::encode(file.metadata.sha256_hash()));

    // Conditional GET: reply 304 without a body when the client already
    // holds the current version.
    if request_headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',')
                .any(|t| t.trim().trim_start_matches("W/") == etag)
        })
    {
        return (
            StatusCode::NOT_MODIFIED,
            [
                (header::ETAG, etag),
                (header::CACHE_CONTROL, CACHE_CONTROL_VALUE.to_string()),
            ],
        )
            .into_response();
    }

    let mime = mime_guess::from_path(path).first_or_octet_stream();
    // Serve the embedded bytes without copying: in release builds the data
    // is `Cow::Borrowed(&'static [u8])`, which converts to a zero-copy body.
    let body = match file.data {
        Cow::Borrowed(bytes) => Body::from(bytes),
        Cow::Owned(bytes) => Body::from(bytes),
    };

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, mime.as_ref().to_string()),
            (header::ETAG, etag),
            (header::CACHE_CONTROL, CACHE_CONTROL_VALUE.to_string()),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_index_with_etag_and_cache_control() {
        let response = serve_file("index.html", &HeaderMap::new());
        assert_eq!(response.status(), StatusCode::OK);
        let etag = response.headers().get(header::ETAG).expect("ETag header");
        assert!(etag.to_str().unwrap().starts_with('"'));
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-cache"
        );
    }

    #[test]
    fn returns_304_when_if_none_match_matches() {
        let response = serve_file("index.html", &HeaderMap::new());
        let etag = response
            .headers()
            .get(header::ETAG)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        let mut headers = HeaderMap::new();
        headers.insert(header::IF_NONE_MATCH, etag.parse().unwrap());
        let cached = serve_file("index.html", &headers);
        assert_eq!(cached.status(), StatusCode::NOT_MODIFIED);
        assert!(cached.headers().get(header::ETAG).is_some());
    }

    #[test]
    fn stale_etag_gets_full_response() {
        let mut headers = HeaderMap::new();
        headers.insert(header::IF_NONE_MATCH, "\"deadbeef\"".parse().unwrap());
        let response = serve_file("index.html", &headers);
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn missing_file_is_404() {
        let response = serve_file("nope.js", &HeaderMap::new());
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
