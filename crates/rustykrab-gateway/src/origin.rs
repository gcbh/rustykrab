use std::collections::HashSet;

use axum::extract::Request;
use axum::http::{header, HeaderValue, Method, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::Response;

/// Set of allowed WebSocket/HTTP origins.
///
/// Prevents the ClawJacked class of attacks (CVE-2026-32025) where a
/// malicious website opens a WebSocket to localhost and hijacks the agent.
#[derive(Debug, Clone)]
pub struct OriginPolicy {
    allowed: HashSet<String>,
}

impl OriginPolicy {
    /// Create a policy that only allows the given origins.
    /// An empty set adds no non-loopback origins. API requests still need
    /// an allowed Origin or the bounded same-origin browser-read proof.
    pub fn new(allowed: impl IntoIterator<Item = String>) -> Self {
        Self {
            allowed: allowed.into_iter().collect(),
        }
    }

    /// Check whether a given origin string is permitted.
    ///
    /// Allows HTTP and HTTPS variants of loopback addresses, including
    /// IPv6 `[::1]`, so that HTTPS-enabled local dev servers and IPv6
    /// clients are not rejected.
    pub fn is_allowed(&self, origin: &str) -> bool {
        // Always allow loopback origins served by us.
        if origin.starts_with("http://127.0.0.1:")
            || origin.starts_with("https://127.0.0.1:")
            || origin.starts_with("http://localhost:")
            || origin.starts_with("https://localhost:")
            || origin.starts_with("http://[::1]:")
            || origin.starts_with("https://[::1]:")
        {
            return true;
        }
        // Compare normalised forms on both sides: the allowlist is
        // normalised when parsed, so comparing a raw header against it
        // would reject an origin differing only in host case.
        match Self::normalize(origin) {
            Some(normalised) => self.allowed.contains(&normalised),
            None => false,
        }
    }
}

impl Default for OriginPolicy {
    fn default() -> Self {
        // By default, only allow our own loopback origins.
        Self::new(std::iter::empty::<String>())
    }
}

impl OriginPolicy {
    /// Policy built from `RUSTYKRAB_ALLOWED_ORIGINS` — a comma-separated
    /// list of exact origins (`https://mac.tailnet.ts.net`).
    ///
    /// Loopback is always permitted, so this is only needed for clients
    /// that reach the gateway by another name. The Apollo app is the
    /// motivating case: it sends its own `Origin`, and every `/api`
    /// request from it was rejected with `403` until its tailnet origin
    /// could be allowed.
    ///
    /// Entries are normalised (trimmed, trailing `/` removed, host
    /// lowercased) because an origin that differs only in punctuation is
    /// an operator typo, not a security boundary. A malformed entry is
    /// skipped with a warning rather than silently widening or narrowing
    /// the policy.
    pub fn from_env() -> Self {
        let raw = match std::env::var("RUSTYKRAB_ALLOWED_ORIGINS") {
            Ok(v) => v,
            Err(_) => return Self::default(),
        };
        let allowed: Vec<String> = raw
            .split(',')
            .filter_map(|entry| match Self::normalize(entry) {
                Some(origin) => Some(origin),
                None => {
                    if !entry.trim().is_empty() {
                        tracing::warn!(
                            entry = entry.trim(),
                            "ignoring malformed RUSTYKRAB_ALLOWED_ORIGINS entry \
                             (expected scheme://host[:port])"
                        );
                    }
                    None
                }
            })
            .collect();
        if !allowed.is_empty() {
            tracing::info!(origins = ?allowed, "additional origins allowed");
        }
        Self::new(allowed)
    }

    /// `scheme://host[:port]`, lowercased host, no trailing slash or path.
    fn normalize(entry: &str) -> Option<String> {
        let entry = entry.trim().trim_end_matches('/');
        let (scheme, rest) = entry.split_once("://")?;
        if !matches!(scheme, "http" | "https") || rest.is_empty() {
            return None;
        }
        // An origin is scheme + host + port; anything after the authority
        // is not part of it.
        let authority = rest.split(['/', '?', '#']).next()?;
        if authority.is_empty() {
            return None;
        }
        Some(format!("{}://{}", scheme, authority.to_ascii_lowercase()))
    }
}

/// Browsers omit Origin on same-origin GETs. Fetch Metadata's Sec-* headers
/// are browser-controlled: https://www.w3.org/TR/fetch-metadata/
/// Accept only an authenticated
/// read whose browser-controlled Fetch Metadata and referrer agree with this
/// gateway's authority and allowed origin. Commands still require Origin.
/// Sec-* headers cannot be set by page JavaScript:
/// https://www.w3.org/TR/fetch-metadata/#sec-prefix
fn same_origin_browser_read(request: &Request, policy: &OriginPolicy) -> bool {
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        return false;
    }
    let headers = request.headers();
    let value = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    if value("sec-fetch-site") != Some("same-origin")
        || !matches!(value("sec-fetch-mode"), Some("cors" | "same-origin"))
        || value("sec-fetch-dest") != Some("empty")
    {
        return false;
    }
    let Some(referer) = value("referer").and_then(|s| s.parse::<Uri>().ok()) else {
        return false;
    };
    let (Some(scheme), Some(authority), Some(host)) =
        (referer.scheme_str(), referer.authority(), value("host"))
    else {
        return false;
    };
    matches!(scheme, "http" | "https")
        && authority.as_str().eq_ignore_ascii_case(host)
        && policy.is_allowed(&format!("{scheme}://{authority}"))
}

/// Axum middleware that validates the Origin header and adds CORS response headers.
///
/// For sensitive endpoints (/api/ and /webhook/), the Origin header is
/// mandatory except for same-origin browser reads proven by Fetch Metadata
/// and a matching allowed referrer. Omitting both proofs remains a refusal.
///
/// When the origin is allowed, CORS headers are added to the response
/// so that legitimate cross-origin requests from browsers succeed.
pub async fn origin_check_middleware(
    state: axum::extract::State<crate::AppState>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let path = request.uri().path();
    let is_sensitive = path.starts_with("/api/") || path.starts_with("/webhook/");

    let allowed_origin = match request.headers().get(header::ORIGIN) {
        Some(origin) => {
            let origin_str = origin.to_str().unwrap_or("");
            if !state.origin_policy.is_allowed(origin_str) {
                tracing::warn!(
                    origin = origin_str,
                    "rejected request from disallowed origin"
                );
                return Err(StatusCode::FORBIDDEN);
            }
            // Clone the already-parsed HeaderValue to echo back later —
            // cheaper than re-parsing the origin string per response.
            Some(origin.clone())
        }
        None if is_sensitive && same_origin_browser_read(&request, &state.origin_policy) => None,
        None if is_sensitive => {
            tracing::warn!(
                path = %path,
                "rejected request without Origin header on sensitive endpoint"
            );
            return Err(StatusCode::FORBIDDEN);
        }
        None => None,
    };

    let mut response = next.run(request).await;

    // Add CORS headers when the origin was validated.
    if let Some(origin) = allowed_origin {
        let headers = response.headers_mut();
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET, POST, PUT, DELETE, PATCH, OPTIONS"),
        );
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static("Content-Type, Authorization"),
        );
    }

    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_read_requires_metadata_allowed_referrer_and_matching_authority() {
        fn request(method: &str, site: &str, referer: &str, mode: &str, dest: &str) -> Request {
            Request::builder()
                .method(method)
                .uri("/api/monitor")
                .header("Host", "127.0.0.1:3000")
                .header("Sec-Fetch-Site", site)
                .header("Sec-Fetch-Mode", mode)
                .header("Sec-Fetch-Dest", dest)
                .header("Referer", referer)
                .body(axum::body::Body::empty())
                .unwrap()
        }
        let policy = OriginPolicy::default();
        assert!(same_origin_browser_read(
            &request(
                "GET",
                "same-origin",
                "http://127.0.0.1:3000/monitor.html",
                "cors",
                "empty"
            ),
            &policy
        ));
        assert!(same_origin_browser_read(
            &request(
                "HEAD",
                "same-origin",
                "http://127.0.0.1:3000/",
                "same-origin",
                "empty"
            ),
            &policy
        ));
        for method in ["POST", "PUT", "PATCH", "DELETE"] {
            assert!(!same_origin_browser_read(
                &request(
                    method,
                    "same-origin",
                    "http://127.0.0.1:3000/",
                    "cors",
                    "empty"
                ),
                &policy
            ));
        }
        for site in ["cross-site", "same-site", "none", ""] {
            assert!(!same_origin_browser_read(
                &request("GET", site, "http://127.0.0.1:3000/", "cors", "empty"),
                &policy
            ));
        }
        for referer in [
            "http://outside.example/",
            "http://127.0.0.1:4000/",
            "not a uri",
            "",
        ] {
            assert!(!same_origin_browser_read(
                &request("GET", "same-origin", referer, "cors", "empty"),
                &policy
            ));
        }
        assert!(!same_origin_browser_read(
            &request(
                "GET",
                "same-origin",
                "http://127.0.0.1:3000/",
                "navigate",
                "document"
            ),
            &policy
        ));
    }

    /// Guards against a regression that would silently expose the gateway:
    /// with no configuration, only loopback is allowed.
    #[test]
    fn default_policy_allows_only_loopback() {
        let policy = OriginPolicy::default();
        assert!(policy.is_allowed("http://127.0.0.1:3000"));
        assert!(policy.is_allowed("http://localhost:8080"));
        assert!(policy.is_allowed("https://[::1]:3000"));
        assert!(!policy.is_allowed("https://mac.tailnet.ts.net"));
        assert!(!policy.is_allowed("https://evil.example.com"));
    }

    #[test]
    fn configured_origins_are_allowed_alongside_loopback() {
        let policy = OriginPolicy::new(["https://mac.tailnet.ts.net".to_string()]);
        assert!(policy.is_allowed("https://mac.tailnet.ts.net"));
        assert!(policy.is_allowed("http://127.0.0.1:3000"));
        assert!(!policy.is_allowed("https://other.tailnet.ts.net"));
    }

    #[test]
    fn normalization_ignores_trailing_slash_and_host_case() {
        assert_eq!(
            OriginPolicy::normalize("  https://Mac.Tailnet.TS.net/  "),
            Some("https://mac.tailnet.ts.net".to_string())
        );
        // A path is not part of an origin.
        assert_eq!(
            OriginPolicy::normalize("https://mac.ts.net/api/health"),
            Some("https://mac.ts.net".to_string())
        );
        // Ports are part of it and must survive.
        assert_eq!(
            OriginPolicy::normalize("http://mac.ts.net:8443"),
            Some("http://mac.ts.net:8443".to_string())
        );
    }

    #[test]
    fn an_origin_matches_regardless_of_host_case() {
        let policy = OriginPolicy::new(["https://mac.tailnet.ts.net".to_string()]);
        assert!(policy.is_allowed("https://MAC.Tailnet.ts.net"));
    }

    #[test]
    fn malformed_entries_are_dropped_not_widened() {
        // Nothing here should end up permitting anything.
        let policy = OriginPolicy::new(
            ["not-a-url", "ftp://mac.ts.net", "https://", ""]
                .iter()
                .filter_map(|e| OriginPolicy::normalize(e))
                .collect::<Vec<_>>(),
        );
        assert!(!policy.is_allowed("not-a-url"));
        assert!(!policy.is_allowed("ftp://mac.ts.net"));
        assert!(!policy.is_allowed("https://mac.ts.net"));
        // Loopback still works.
        assert!(policy.is_allowed("http://127.0.0.1:3000"));
    }

    #[test]
    fn scheme_must_match_too() {
        let policy = OriginPolicy::new(["https://mac.ts.net".to_string()]);
        // Downgrading to http is a different origin, and allowing it
        // would defeat the point of pinning https.
        assert!(!policy.is_allowed("http://mac.ts.net"));
    }
}
