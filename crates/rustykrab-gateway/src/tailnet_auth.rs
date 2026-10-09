//! Opt-in browser authentication through the operator's Tailscale Serve proxy.
//! Trust requires a kernel-observed loopback peer, the pinned HTTPS authority,
//! and an explicitly allowed login. Unconfigured/empty policy grants nothing.
//! Local processes on the Serve host are inside this deployment trust boundary.
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, Uri};
use axum::response::IntoResponse;
use axum::{Extension, Json, Router};
use rustykrab_store::Principal;
use serde::Serialize;
use std::net::SocketAddr;

use crate::AppState;

#[derive(Debug, Clone)]
pub struct TailnetAuthPolicy {
    authority: String,
    allowed_logins: Vec<String>,
}

impl TailnetAuthPolicy {
    /// Pin one private Serve origin and a nonempty owner allowlist.
    pub fn new(origin: &str, allowed_logins: Vec<String>) -> Result<Self, String> {
        let uri = origin
            .trim()
            .parse::<Uri>()
            .map_err(|_| "Invalid tailnet auth origin")?;
        let authority = uri.authority().ok_or("Tailnet auth origin needs a host")?;
        if uri.scheme_str() != Some("https")
            || !authority.host().to_ascii_lowercase().ends_with(".ts.net")
            || authority.as_str().contains('@')
            || !matches!(
                uri.path_and_query().map(|p| p.as_str()),
                None | Some("" | "/")
            )
        {
            return Err(
                "Tailnet auth requires an exact HTTPS .ts.net origin without a path".into(),
            );
        }
        let logins: Vec<_> = allowed_logins
            .into_iter()
            .map(|login| login.trim().to_ascii_lowercase())
            .filter(|login| !login.is_empty())
            .collect();
        if logins.is_empty()
            || logins
                .iter()
                .any(|s| s.len() > 254 || s.contains('*') || s.chars().any(char::is_whitespace))
        {
            return Err("Tailnet auth requires explicit RUSTYKRAB_TAILNET_USERS logins".into());
        }
        Ok(Self {
            authority: authority.as_str().to_ascii_lowercase(),
            allowed_logins: logins,
        })
    }

    pub(crate) fn principal(&self, request: &Request) -> Option<Principal> {
        let peer = request.extensions().get::<ConnectInfo<SocketAddr>>()?;
        if !peer.0.ip().is_loopback() {
            return None;
        }
        let unique = |name: &str| {
            let mut values = request.headers().get_all(name).iter();
            let value = values.next()?.to_str().ok()?;
            if values.next().is_some() {
                return None;
            }
            Some(value)
        };
        if !unique("host")?.eq_ignore_ascii_case(&self.authority) {
            return None;
        }
        let login = unique("tailscale-user-login")?.trim().to_ascii_lowercase();
        if !self.allowed_logins.contains(&login) {
            return None;
        }
        Some(Principal::Tailnet { login })
    }
}

/// Same verification used by authentication and its anti-brute-force exemption.
/// A supplied bad credential is never silently upgraded to network identity.
pub(crate) fn principal(state: &AppState, request: &Request) -> Option<Principal> {
    if request.headers().contains_key(header::AUTHORIZATION) {
        return None;
    }
    state.tailnet_auth.as_ref()?.principal(request)
}

#[derive(Serialize)]
struct AccessStatus {
    authenticated: bool,
    method: Option<&'static str>,
    identity: Option<String>,
    tailscale_enabled: bool,
}

/// No credentials or allowed-account list are returned. An unauthenticated
/// browser can discover its sign-in method; all other protected APIs stay closed.
async fn status(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
) -> impl IntoResponse {
    let principal = principal.map(|Extension(p)| p);
    let method = principal.as_ref().map(|p| match p {
        Principal::Tailnet { .. } => "tailscale",
        _ => "token",
    });
    let identity = principal.as_ref().map(|p| match p {
        Principal::Tailnet { login } => login.clone(),
        Principal::Device { name, .. } => name.clone(),
        Principal::Master => "Access token".into(),
    });
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(AccessStatus {
            authenticated: principal.is_some(),
            method,
            identity,
            tailscale_enabled: state.tailnet_auth.is_some(),
        }),
    )
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new().route("/api/access", axum::routing::get(status))
}

#[cfg(test)]
mod tests;
