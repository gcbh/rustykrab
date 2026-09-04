//! Browser navigation policy shared by requested and observed navigations.
//!
//! A pre-navigation check is not enough: clicks, redirects, form submits and
//! JavaScript can move a tab after the requested URL was approved. Callers use
//! the observed form after every navigation-capable action and quarantine any
//! destination it rejects.

use super::config::SsrfPolicy;
use crate::security;
use chromiumoxide::Page;
use serde_json::{json, Value};
use std::time::Duration;

const POLICY_CHECK_BUDGET: Duration = Duration::from_secs(3);
const QUARANTINE_BUDGET: Duration = Duration::from_secs(3);

/// Browser-owned URLs which do not cross a network boundary.
pub fn is_internal_url(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    match parsed.scheme() {
        // Fragments are legitimate same-document navigation. `about:srcdoc` is
        // the browser-owned URL of an iframe `srcdoc` document.
        "about" => matches!(parsed.path(), "blank" | "srcdoc"),
        "chrome" => matches!(parsed.host_str(), Some("new-tab-page" | "newtab")),
        _ => false,
    }
}

/// Validate a URL the agent explicitly asked Chrome to load.
pub async fn validate_requested(url: &str, policy: &SsrfPolicy) -> Result<(), String> {
    enforce_domain_policy(url, policy)?;
    security::validate_url_with_overrides(
        url,
        &policy.hostname_allowlist,
        policy.allow_private_network,
    )
    .await
    .map(|_| ())
}

/// Validate the URL Chrome actually reached after a redirect or page action.
/// `data:` and `blob:` cannot issue an SSRF request themselves and are valid
/// observed renderer destinations, but remain unavailable as requested URLs.
pub async fn validate_observed(url: &str, policy: &SsrfPolicy) -> Result<(), String> {
    if is_internal_url(url) {
        return Ok(());
    }
    let parsed = url::Url::parse(url).map_err(|error| format!("invalid browser URL: {error}"))?;
    if matches!(parsed.scheme(), "data" | "blob") {
        return Ok(());
    }
    validate_requested(url, policy).await
}

/// Check the current page URL and move a rejected destination to
/// `about:blank` before any page content is returned to the agent.
pub async fn enforce_page(page: &Page, policy: &SsrfPolicy) -> Value {
    let target_id = page.target_id().inner().clone();
    let url = match tokio::time::timeout(POLICY_CHECK_BUDGET, page.url()).await {
        Ok(Ok(Some(url))) => url,
        Ok(Ok(None)) => {
            return json!({
                "status": "unverified",
                "targetId": target_id,
                "reason": "CDP returned no current URL"
            })
        }
        Ok(Err(error)) => {
            return json!({
                "status": "unverified",
                "targetId": target_id,
                "reason": format!("failed to read current URL: {error}")
            })
        }
        Err(_) => {
            return json!({
                "status": "unverified",
                "targetId": target_id,
                "reason": "current URL probe exceeded the policy budget"
            })
        }
    };

    let decision = tokio::time::timeout(POLICY_CHECK_BUDGET, validate_observed(&url, policy)).await;
    let reason = match decision {
        Ok(Ok(())) => {
            return json!({
                "status": "allowed",
                "targetId": target_id,
                "url": url,
            })
        }
        Ok(Err(reason)) => reason,
        Err(_) => "URL validation exceeded the policy budget".to_string(),
    };

    let quarantined = matches!(
        tokio::time::timeout(QUARANTINE_BUDGET, page.goto("about:blank")).await,
        Ok(Ok(_))
    );
    tracing::warn!(
        target_id,
        blocked_url = %url,
        quarantined,
        reason = %reason,
        "browser navigation policy blocked an observed destination"
    );
    json!({
        "status": "blocked",
        "targetId": target_id,
        "url": url,
        "reason": reason,
        "quarantined": quarantined,
    })
}

fn enforce_domain_policy(url: &str, policy: &SsrfPolicy) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|error| format!("invalid URL: {error}"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| "URL must have a host".to_string())?
        .trim_end_matches('.')
        .to_ascii_lowercase();

    if policy
        .prohibited_domains
        .iter()
        .any(|pattern| host_matches_pattern(&host, pattern))
    {
        return Err(format!(
            "navigation to '{host}' is prohibited by browser policy"
        ));
    }

    if !policy.allowed_domains.is_empty()
        && !policy
            .allowed_domains
            .iter()
            .any(|pattern| host_matches_pattern(&host, pattern))
    {
        return Err(format!(
            "navigation to '{host}' is outside the browser allowed-domains policy"
        ));
    }

    Ok(())
}

/// Deliberately supports only exact hosts and an explicit leading `*.`.
/// Arbitrary URL globs are difficult to audit and can accidentally match a
/// hostile sibling domain.
fn host_matches_pattern(host: &str, pattern: &str) -> bool {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    let pattern = pattern.trim().trim_end_matches('.').to_ascii_lowercase();
    if let Some(suffix) = pattern.strip_prefix("*.") {
        !suffix.is_empty() && (host == suffix || host.ends_with(&format!(".{suffix}")))
    } else {
        !pattern.is_empty() && host == pattern
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_patterns_are_boundary_aware() {
        assert!(host_matches_pattern("example.com", "*.example.com"));
        assert!(host_matches_pattern("a.example.com", "*.example.com"));
        assert!(!host_matches_pattern("evil-example.com", "*.example.com"));
        assert!(!host_matches_pattern(
            "example.com.evil.test",
            "*.example.com"
        ));
        assert!(host_matches_pattern("WWW.Example.com", "www.example.com"));
    }

    #[test]
    fn deny_rules_outrank_allow_rules() {
        let policy = SsrfPolicy {
            allowed_domains: vec!["*.example.com".into()],
            prohibited_domains: vec!["admin.example.com".into()],
            ..Default::default()
        };
        let error = enforce_domain_policy("https://admin.example.com/", &policy).unwrap_err();
        assert!(error.contains("prohibited"), "{error}");
        assert!(enforce_domain_policy("https://www.example.com/", &policy).is_ok());
    }

    #[test]
    fn internal_urls_allow_fragments_without_allowing_arbitrary_about_pages() {
        assert!(is_internal_url("about:blank#after-click"));
        assert!(is_internal_url("about:srcdoc"));
        assert!(is_internal_url("chrome://newtab/#ignored"));
        assert!(!is_internal_url("about:config"));
        assert!(!is_internal_url("about:blank.evil"));
    }

    #[tokio::test]
    async fn requested_data_urls_remain_blocked_but_observed_ones_are_allowed() {
        let policy = SsrfPolicy::default();
        assert!(validate_requested("data:text/plain,hello", &policy)
            .await
            .is_err());
        assert!(validate_observed("data:text/plain,hello", &policy)
            .await
            .is_ok());
    }
}
