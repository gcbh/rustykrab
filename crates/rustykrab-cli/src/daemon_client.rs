//! The loopback client the terminal commands (`chat`, `work`) use to reach
//! a running daemon: the gateway URL from `RUSTYKRAB_GATEWAY_URL`, the
//! daemon's bearer token, and the gateway's own origin on every request.
//! One copy, so the two commands cannot drift on how they find, prove
//! themselves to, or address the daemon.

use std::path::Path;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, ORIGIN};
use reqwest::Url;

pub(crate) const DEFAULT_GATEWAY_URL: &str = "http://127.0.0.1:3000";

/// The daemon's base URL: `RUSTYKRAB_GATEWAY_URL`, else the default
/// loopback, which must be an http(s) URL with a host.
pub(crate) fn gateway_url() -> anyhow::Result<Url> {
    let raw = std::env::var("RUSTYKRAB_GATEWAY_URL").unwrap_or_else(|_| DEFAULT_GATEWAY_URL.into());
    parse(&raw)
}

fn parse(raw: &str) -> anyhow::Result<Url> {
    let url = Url::parse(raw)
        .map_err(|e| anyhow::anyhow!("invalid RUSTYKRAB_GATEWAY_URL `{raw}`: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
        anyhow::bail!("RUSTYKRAB_GATEWAY_URL must be an http(s) URL with a host");
    }
    Ok(url)
}

/// The HTTP origin the gateway's CSRF boundary expects.
///
/// The gateway deliberately requires `Origin` on sensitive `/api` routes,
/// including requests from non-browser clients. Supplying the configured
/// gateway's own origin preserves that boundary while allowing this trusted
/// loopback client to use the API.
pub(crate) fn gateway_origin(base: &str) -> anyhow::Result<HeaderValue> {
    let url = parse(base)?;
    HeaderValue::from_str(&url.origin().ascii_serialization())
        .map_err(|e| anyhow::anyhow!("invalid gateway origin: {e}"))
}

/// An HTTP client that carries the bearer token and the gateway's origin
/// on every request.
pub(crate) fn client(
    base: &Url,
    token: &str,
    timeout: Duration,
) -> anyhow::Result<reqwest::Client> {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|e| anyhow::anyhow!("invalid auth token: {e}"))?,
    );
    headers.insert(ORIGIN, gateway_origin(base.as_str())?);
    Ok(reqwest::Client::builder()
        .default_headers(headers)
        .timeout(timeout)
        .build()?)
}

/// The daemon's base URL and a client for it, authenticated.
pub(crate) async fn connect(
    data_dir: &Path,
    timeout: Duration,
) -> anyhow::Result<(Url, reqwest::Client)> {
    let base = gateway_url()?;
    let token = resolve_auth_token(data_dir).await?;
    let client = client(&base, &token, timeout)?;
    Ok((base, client))
}

// The client must hold the same bearer token the daemon accepts, found the
// way the daemon finds it: env, then keychain, then store. If the store is
// locked (the daemon holds it), only env and keychain are tried; the user
// can always set RUSTYKRAB_AUTH_TOKEN explicitly.
pub(crate) async fn resolve_auth_token(data_dir: &Path) -> anyhow::Result<String> {
    if let Ok(v) = std::env::var("RUSTYKRAB_AUTH_TOKEN") {
        let v = v.trim();
        if !v.is_empty() {
            return Ok(v.to_string());
        }
    }

    let spec = rustykrab_store::registry::lookup("rustykrab_auth_token")
        .ok_or_else(|| anyhow::anyhow!("auth-token spec missing from registry"))?;

    if rustykrab_store::keychain::keychain_available() {
        if let Ok(Some(cred)) = rustykrab_store::keychain::get_credential(
            rustykrab_store::registry::keychain_service(),
            spec.keychain_account,
        ) {
            return Ok(cred.value);
        }
    }

    // Last resort: open the store. Fails if the daemon holds an exclusive
    // lock on it, which is fine: the message below says what to set.
    let db_path = data_dir.join("db");
    if db_path.exists() {
        if let Ok(master_key) = rustykrab_store::keychain::resolve_master_key() {
            if let Ok(store) = rustykrab_store::Store::open(&db_path, master_key) {
                if let Ok(v) = store.secrets().get(spec.store_name).await {
                    return Ok(v);
                }
            }
        }
    }

    anyhow::bail!(
        "could not resolve the auth token. Set RUSTYKRAB_AUTH_TOKEN to the value \
         the daemon printed at startup, or run `rustykrab-cli keychain status` \
         to inspect what's stored."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_origin_uses_only_scheme_authority_and_port() {
        assert_eq!(
            gateway_origin("https://Example.COM:8443/a/path")
                .unwrap()
                .to_str()
                .unwrap(),
            "https://example.com:8443"
        );
    }

    #[test]
    fn gateway_origin_accepts_the_default_loopback_url() {
        assert_eq!(
            gateway_origin(DEFAULT_GATEWAY_URL)
                .unwrap()
                .to_str()
                .unwrap(),
            DEFAULT_GATEWAY_URL
        );
    }

    #[test]
    fn gateway_origin_rejects_non_http_urls() {
        let error = gateway_origin("file:///tmp/rustykrab.sock").unwrap_err();
        assert!(error.to_string().contains("must be an http(s) URL"));
    }

    #[test]
    fn a_client_carries_the_token_and_the_gateways_origin() {
        let base = parse("http://127.0.0.1:3999").unwrap();
        assert!(client(&base, "secret-token", Duration::from_secs(5)).is_ok());
        assert!(client(&base, "bad\ntoken", Duration::from_secs(5)).is_err());
    }
}
