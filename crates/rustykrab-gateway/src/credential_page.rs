//! A page the user opens to hand over a credential the agent lacks.
//!
//! The agent cannot ask for a password in chat — a chat message has no
//! secure field, and whatever the user types is then sitting in the
//! transcript of whichever channel they answered on. So the agent sends a
//! link instead, and this is what the link opens.
//!
//! ## Why this is server-rendered HTML and not part of the SPA
//!
//! Two properties fall out of plain HTML that the JSON API cannot offer
//! today:
//!
//! - **It works in a browser at all.** `/api/` is `is_sensitive`, so a
//!   request without an `Origin` header is refused — and browsers omit
//!   `Origin` on same-origin GETs. This route is outside that prefix, and
//!   the form POST carries an `Origin` natively, so neither hits the wall.
//! - **No JavaScript touches the value.** The password goes from the input
//!   straight into a form POST. Nothing reads it into a variable, so there
//!   is nothing to accidentally log, cache, or leave in the DOM.
//!
//! ## What guards it
//!
//! Two independent things, per the deployment model — the gateway binds
//! loopback and is fronted by `tailscale serve`:
//!
//! 1. **Tailnet identity.** `tailscale serve` injects `Tailscale-User-Login`
//!    for the authenticated tailnet user. Requests without it are refused
//!    unless anonymous access is explicitly enabled for local development.
//! 2. **A one-time token**, bound to a single request, hashed at rest, and
//!    dead the moment the request is answered or the TTL passes.
//!
//! Either alone would be weaker than it looks. The header is only
//! trustworthy because nothing but the tailnet front end can reach the
//! listener, and a link that travels through a chat message can be read by
//! anyone who later reads that chat.

use axum::extract::{Form, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use rustykrab_store::CredentialRequest;
use std::collections::HashMap;

use crate::AppState;

/// Header `tailscale serve` sets for the authenticated tailnet user.
const TAILNET_USER_HEADER: &str = "tailscale-user-login";

/// Who may open a credential page.
#[derive(Debug, Clone, Default)]
pub struct PageIdentityPolicy {
    /// Accept requests with no tailnet identity at all. Off by default and
    /// only for loopback development: with `tailscale serve` in front, a
    /// request that reaches us without the header did not come through it.
    pub allow_anonymous: bool,
    /// Logins permitted, when the tailnet has more than one member. Empty
    /// means any authenticated tailnet user.
    pub allowed_logins: Vec<String>,
}

impl PageIdentityPolicy {
    pub fn from_env() -> Self {
        let allow_anonymous = matches!(
            std::env::var("RUSTYKRAB_CREDENTIAL_PAGE_ANONYMOUS")
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase()
                .as_str(),
            "1" | "true" | "on" | "yes"
        );
        let allowed_logins = std::env::var("RUSTYKRAB_TAILNET_USERS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        Self {
            allow_anonymous,
            allowed_logins,
        }
    }

    /// Decide whether these headers may open a credential page.
    ///
    /// Fails closed: an absent header is a refusal unless anonymous access
    /// was turned on deliberately.
    fn permits(&self, headers: &HeaderMap) -> bool {
        let login = headers
            .get(TAILNET_USER_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.trim().to_ascii_lowercase())
            .filter(|v| !v.is_empty());

        match login {
            Some(login) => self.allowed_logins.is_empty() || self.allowed_logins.contains(&login),
            None => self.allow_anonymous,
        }
    }
}

pub fn routes() -> axum::Router<AppState> {
    axum::Router::new().route("/c/{token}", axum::routing::get(show).post(submit))
}

/// Everything the user is refused with looks the same.
///
/// A wrong token, an expired one, one already answered, and one belonging
/// to somebody else all render this. Distinguishing them would turn the
/// page into an oracle for which links exist.
fn refused() -> Response {
    (
        StatusCode::NOT_FOUND,
        Html(page(
            "Link expired",
            "<p>This link is no longer valid. It may already have been used, \
             or it may have expired.</p><p>Ask the agent to send a new one.</p>",
        )),
    )
        .into_response()
}

async fn show(
    State(state): State<AppState>,
    Path(token): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !state.credential_page_policy.permits(&headers) {
        tracing::warn!("credential page refused: no permitted tailnet identity");
        return refused();
    }
    let Ok(Some(req)) = state.store.credential_requests().find_by_link(&token).await else {
        return refused();
    };
    Html(form_page(&req, &token, None)).into_response()
}

async fn submit(
    State(state): State<AppState>,
    Path(token): Path<String>,
    headers: HeaderMap,
    Form(values): Form<HashMap<String, String>>,
) -> Response {
    if !state.credential_page_policy.permits(&headers) {
        tracing::warn!("credential page submit refused: no permitted tailnet identity");
        return refused();
    }
    let Ok(Some(req)) = state.store.credential_requests().find_by_link(&token).await else {
        return refused();
    };

    // Re-render rather than reject: the user has just typed a password, and
    // throwing it away over a blank second field is a poor trade.
    let missing: Vec<&str> = req
        .fields
        .iter()
        .filter(|f| {
            values
                .get(&f.key)
                .map(|v| v.trim().is_empty())
                .unwrap_or(true)
        })
        .map(|f| f.key.as_str())
        .collect();
    if !missing.is_empty() {
        return Html(form_page(&req, &token, Some("Every field is needed."))).into_response();
    }

    let supplied: Vec<(String, String)> = req
        .fields
        .iter()
        .filter_map(|f| values.get(&f.key).map(|v| (f.key.clone(), v.clone())))
        .collect();

    let by = headers
        .get(TAILNET_USER_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("credential page");

    match state
        .store
        .credential_requests()
        .fulfil(&req.id, &supplied, by)
        .await
    {
        Ok(()) => Html(page(
            "Saved",
            "<p>Saved. You can close this page — the agent has what it needs \
             and will carry on.</p>",
        ))
        .into_response(),
        Err(e) => {
            // The value is not echoed back into the retry form: it has been
            // sent once already and the failure may well be the store.
            tracing::error!(error = %e, "credential page could not fulfil the request");
            Html(form_page(
                &req,
                &token,
                Some("That could not be saved. Try once more."),
            ))
            .into_response()
        }
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn form_page(req: &CredentialRequest, token: &str, error: Option<&str>) -> String {
    let service = req.service.as_deref().unwrap_or(&req.name);
    let mut body = String::new();
    body.push_str(&format!("<h1>{} needs a credential</h1>", esc(service)));
    if let Some(reason) = req.reason.as_deref().filter(|r| !r.is_empty()) {
        body.push_str(&format!("<p class=\"why\">{}</p>", esc(reason)));
    }
    if let Some(err) = error {
        body.push_str(&format!("<p class=\"err\">{}</p>", esc(err)));
    }
    body.push_str(&format!(
        "<form method=\"post\" action=\"/c/{}\" autocomplete=\"off\">",
        esc(token)
    ));
    for f in &req.fields {
        let kind = if f.secret { "password" } else { "text" };
        body.push_str(&format!(
            "<label>{}<input name=\"{}\" type=\"{}\" \
             autocapitalize=\"none\" autocorrect=\"off\" spellcheck=\"false\" \
             placeholder=\"{}\" required></label>",
            esc(&f.label),
            esc(&f.key),
            kind,
            esc(f.hint.as_deref().unwrap_or("")),
        ));
    }
    body.push_str("<button type=\"submit\">Save</button></form>");
    body.push_str(
        "<p class=\"note\">Sent once, over your tailnet, straight into this \
         Mac's encrypted store. This link works only now, and only once.</p>",
    );
    page(&format!("{service} credential"), &body)
}

fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<meta name=\"referrer\" content=\"no-referrer\">\
<title>{}</title><style>\
:root{{color-scheme:light dark}}\
body{{font:16px/1.5 -apple-system,BlinkMacSystemFont,'SF Pro Text',sans-serif;\
margin:0;padding:24px;max-width:26rem;margin-inline:auto}}\
h1{{font-size:1.35rem;margin:0 0 .25rem}}\
.why{{color:#666;margin:.25rem 0 1.25rem}}\
.err{{color:#c0392b;margin:.5rem 0}}\
.note{{color:#666;font-size:.82rem;margin-top:1.25rem}}\
label{{display:block;margin-bottom:.85rem;font-size:.85rem;color:#666}}\
input{{display:block;width:100%;margin-top:.3rem;padding:.65rem .7rem;\
font-size:1rem;border:1px solid #bbb;border-radius:.5rem;box-sizing:border-box}}\
button{{width:100%;padding:.7rem;font-size:1rem;border:0;border-radius:.5rem;\
background:#0a84ff;color:#fff;font-weight:600}}\
</style></head><body>{}</body></html>",
        esc(title),
        body
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(login: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(l) = login {
            h.insert(TAILNET_USER_HEADER, HeaderValue::from_str(l).unwrap());
        }
        h
    }

    #[test]
    fn no_tailnet_identity_is_refused_by_default() {
        let p = PageIdentityPolicy::default();
        assert!(!p.permits(&headers(None)));
        assert!(p.permits(&headers(Some("someone@example.com"))));
    }

    #[test]
    fn anonymous_is_only_allowed_when_turned_on() {
        let p = PageIdentityPolicy {
            allow_anonymous: true,
            ..Default::default()
        };
        assert!(p.permits(&headers(None)));
    }

    #[test]
    fn an_allowlist_excludes_other_tailnet_members() {
        let p = PageIdentityPolicy {
            allow_anonymous: false,
            allowed_logins: vec!["me@example.com".into()],
        };
        assert!(p.permits(&headers(Some("me@example.com"))));
        assert!(p.permits(&headers(Some("ME@example.com")))); // login case is not identity
        assert!(!p.permits(&headers(Some("someone-else@example.com"))));
        // An empty header is not an identity.
        assert!(!p.permits(&headers(Some("   "))));
    }

    #[test]
    fn rendered_values_are_escaped() {
        let out = page("t", &esc("<script>alert(1)</script>"));
        assert!(!out.contains("<script>alert"));
        assert!(out.contains("&lt;script&gt;"));
    }
}
