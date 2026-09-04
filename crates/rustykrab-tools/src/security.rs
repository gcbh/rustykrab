//! Shared security utilities for tool implementations.
//!
//! Provides path traversal prevention and SSRF protection that are
//! reused across multiple tool implementations.

use std::net::{IpAddr, SocketAddr};
use std::path::{Component, PathBuf};

/// Default safe base directory for file operations.
/// If RUSTYKRAB_WORKSPACE environment variable is set, it is used instead.
pub fn workspace_root() -> PathBuf {
    std::env::var("RUSTYKRAB_WORKSPACE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/tmp/rustykrab"))
        })
}

/// Build the list of blocked path prefixes, including user-home-relative
/// sensitive directories.
fn blocked_path_prefixes() -> Vec<String> {
    let mut prefixes = vec![
        // System sensitive files
        "/etc/shadow".to_string(),
        "/etc/passwd".to_string(),
        "/etc/sudoers".to_string(),
        "/etc/master.passwd".to_string(),
        "/root/.ssh".to_string(),
        "/root".to_string(),
        "/proc".to_string(),
        "/sys".to_string(),
        "/dev".to_string(),
        // macOS system directories
        "/Library".to_string(),
        "/System".to_string(),
    ];

    // Add user-home-relative sensitive directories
    if let Ok(home) = std::env::var("HOME") {
        let sensitive_dirs = [
            ".ssh",
            ".aws",
            ".gnupg",
            ".gpg",
            ".kube",
            ".docker",
            ".config/gcloud",
            ".azure",
            ".credentials",
            ".netrc",
            ".npmrc",
            ".pypirc",
            ".gem/credentials",
        ];
        for dir in &sensitive_dirs {
            prefixes.push(format!("{home}/{dir}"));
        }
    }

    prefixes
}

/// Check if a path string matches any blocked prefix.
fn is_path_blocked(path_str: &str, blocked: &[String]) -> Option<String> {
    for prefix in blocked {
        if path_str.starts_with(prefix.as_str()) {
            return Some(prefix.clone());
        }
    }
    None
}

/// Validate that a file path is safe and within allowed boundaries.
///
/// Returns the canonicalized path if valid, or an error message.
/// Prevents:
/// - Path traversal via `..` components
/// - Symlink escapes (by canonicalizing)
/// - Access to sensitive system directories and user credential files
/// - Access outside the workspace root
pub fn validate_path(path: &str) -> Result<PathBuf, String> {
    let path_buf = PathBuf::from(path);

    // Reject paths with .. components before canonicalization
    for component in path_buf.components() {
        if matches!(component, Component::ParentDir) {
            return Err("path traversal (.. components) is not allowed".into());
        }
    }

    let blocked = blocked_path_prefixes();
    let workspace = workspace_root();

    let path_str = path_buf.to_string_lossy();
    if let Some(prefix) = is_path_blocked(&path_str, &blocked) {
        return Err(format!(
            "access to {prefix} is blocked for security reasons"
        ));
    }

    // For existing files, canonicalize to resolve symlinks and verify location
    if path_buf.exists() {
        let canonical = path_buf
            .canonicalize()
            .map_err(|e| format!("failed to resolve path: {e}"))?;

        let canonical_str = canonical.to_string_lossy();
        if let Some(prefix) = is_path_blocked(&canonical_str, &blocked) {
            return Err(format!(
                "resolved path points to blocked location: {prefix}"
            ));
        }

        // Enforce workspace boundary: canonical path must be under workspace root
        let canonical_workspace = workspace
            .canonicalize()
            .unwrap_or_else(|_| workspace.clone());
        if !canonical.starts_with(&canonical_workspace) {
            return Err(format!(
                "path is outside the workspace boundary ({})",
                canonical_workspace.display()
            ));
        }

        return Ok(canonical);
    }

    // For new files, validate the parent exists and is safe
    if let Some(parent) = path_buf.parent() {
        if parent.exists() {
            let canonical_parent = parent
                .canonicalize()
                .map_err(|e| format!("failed to resolve parent directory: {e}"))?;

            let canonical_str = canonical_parent.to_string_lossy();
            if let Some(prefix) = is_path_blocked(&canonical_str, &blocked) {
                return Err(format!(
                    "parent directory resolves to blocked location: {prefix}"
                ));
            }

            // Enforce workspace boundary for new files too
            let canonical_workspace = workspace
                .canonicalize()
                .unwrap_or_else(|_| workspace.clone());
            if !canonical_parent.starts_with(&canonical_workspace) {
                return Err(format!(
                    "path is outside the workspace boundary ({})",
                    canonical_workspace.display()
                ));
            }
        }
    }

    Ok(path_buf)
}

/// Result of URL validation, including resolved addresses for connection pinning.
#[derive(Debug, Clone)]
pub struct ValidatedUrl {
    /// The resolved socket addresses (DNS resolved and validated).
    /// Callers should connect to these addresses directly to prevent
    /// DNS rebinding (TOCTOU) attacks.
    pub resolved_addrs: Vec<SocketAddr>,
    /// The original host for the Host header.
    pub host: String,
}

/// Hosts the operator has explicitly permitted to resolve to private
/// addresses, from `RUSTYKRAB_SSRF_ALLOW_HOSTS` (comma-separated).
///
/// Empty by default, so the guard is unchanged unless someone opts in.
///
/// This exists because the blanket private-IP block makes the agent
/// unable to reach anything on the machine's own network — including
/// services the operator runs deliberately and reaches by name over a
/// tailnet. Blocking those is not SSRF protection, it is a capability
/// gap: the attack SSRF defends against is the agent being *talked into*
/// reaching somewhere internal, not the operator naming a host up front.
///
/// Matching is exact and case-insensitive. No wildcards: a pattern like
/// `*.example.com` is easy to write and hard to reason about, and the
/// whole value of this list is that its entries are unambiguous.
fn ssrf_allowed_hosts() -> Vec<String> {
    parse_allow_hosts(&std::env::var("RUSTYKRAB_SSRF_ALLOW_HOSTS").unwrap_or_default())
}

/// Split, trim, lowercase, drop blanks. Pure, so it is testable without
/// touching process-global state.
fn parse_allow_hosts(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|h| h.trim().to_ascii_lowercase())
        .filter(|h| !h.is_empty())
        .collect()
}

/// Whether `host_lower` appears in `allowed`.
///
/// Split from the environment lookup so it can be tested without touching
/// process-global state. `std::env::set_var` races every other thread in
/// the process, and a test that mutates the environment will eventually
/// break an unrelated one that reads it.
fn host_matches(allowed: &[String], host_lower: &str) -> bool {
    !host_lower.is_empty() && allowed.iter().any(|h| h == host_lower)
}

/// Validate a URL for SSRF protection.
///
/// Blocks:
/// - Private/internal IP ranges (RFC 1918, link-local, loopback)
/// - Cloud metadata endpoints (169.254.169.254)
/// - Non-HTTP(S) schemes
/// - URLs without a host
///
/// Hosts named in `RUSTYKRAB_SSRF_ALLOW_HOSTS` are exempt from the
/// private-address checks. The cloud metadata endpoint never is. `localhost`
/// requires an exact explicit allowlist entry; the broad private-network flag
/// alone does not permit it.
///
/// Returns resolved socket addresses to prevent DNS rebinding (TOCTOU)
/// attacks. Callers should use the returned addresses to pin connections
/// rather than re-resolving the hostname.
///
/// DNS resolution uses `tokio::net::lookup_host` to avoid blocking the
/// async runtime (fixes ASYNC-H1).
pub async fn validate_url(url: &str) -> Result<ValidatedUrl, String> {
    validate_url_with_allowlist(url, &ssrf_allowed_hosts()).await
}

/// [`validate_url`] with browser-specific policy overrides.
///
/// Browser profiles may explicitly permit named private-network hosts, or all
/// private-network destinations, without weakening the process-wide policy for
/// HTTP tools. Cloud metadata remains blocked in every mode; localhost requires
/// an exact hostname allowlist entry.
pub(crate) async fn validate_url_with_overrides(
    url: &str,
    additional_allowed_hosts: &[String],
    allow_private_network: bool,
) -> Result<ValidatedUrl, String> {
    let mut allowed = ssrf_allowed_hosts();
    for host in additional_allowed_hosts {
        let host = host.trim().to_ascii_lowercase();
        if !host.is_empty() && !allowed.contains(&host) {
            allowed.push(host);
        }
    }
    validate_url_with_policy(url, &allowed, allow_private_network).await
}

/// [`validate_url`] against an explicit allowlist.
///
/// Exists so the guard can be tested without setting an environment
/// variable, which would race every other thread in the process.
pub(crate) async fn validate_url_with_allowlist(
    url: &str,
    allowed: &[String],
) -> Result<ValidatedUrl, String> {
    validate_url_with_policy(url, allowed, false).await
}

async fn validate_url_with_policy(
    url: &str,
    allowed: &[String],
    allow_private_network: bool,
) -> Result<ValidatedUrl, String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("invalid URL: {e}"))?;

    // Only allow http and https schemes
    match parsed.scheme() {
        "http" | "https" => {}
        other => {
            return Err(format!(
                "URL scheme '{other}' is not allowed (only http/https)"
            ))
        }
    }

    let host = parsed.host_str().ok_or("URL must have a host")?;

    // Check for IP-based hosts
    if let Ok(ip) = host.parse::<IpAddr>() {
        // An allowlisted literal IP is checked further down, once
        // `host_lower` exists; here we only reject when it is not.
        if is_private_ip(&ip)
            && !allow_private_network
            && !host_matches(allowed, &host.to_lowercase())
        {
            return Err(format!(
                "requests to private/internal IP addresses ({ip}) are blocked (SSRF protection)"
            ));
        }
    }

    // Block known internal hostnames
    let blocked_hosts = ["metadata.google.internal", "metadata.google.com"];
    let host_lower = host.to_lowercase();
    // Decided before any address check so both the literal-IP and the
    // resolved-address paths honour the same answer.
    let allowlisted = host_matches(allowed, &host_lower);

    for blocked in &blocked_hosts {
        if host_lower == *blocked {
            return Err(format!(
                "requests to '{host}' are blocked (SSRF protection)"
            ));
        }
    }
    if host_lower == "localhost" && !allowlisted {
        return Err(format!(
            "requests to '{host}' are blocked (SSRF protection); add an exact hostnameAllowlist entry to opt in"
        ));
    }

    // Block 169.254.169.254 (AWS/GCP metadata) even as hostname
    if host == "169.254.169.254" {
        return Err("requests to cloud metadata endpoint are blocked (SSRF protection)".into());
    }

    // Resolve hostname and check ALL IPs against private ranges.
    // Return the validated addresses so callers can pin connections,
    // preventing DNS rebinding (TOCTOU) attacks where a second DNS
    // resolution could return a different (internal) IP.
    let port = parsed
        .port()
        .unwrap_or(if parsed.scheme() == "https" { 443 } else { 80 });
    let host_port = format!("{host}:{port}");
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host(&host_port)
        .await
        .map_err(|e| format!("DNS resolution failed for '{host}': {e}"))?
        .collect();

    if addrs.is_empty() {
        return Err(format!("DNS resolution returned no addresses for '{host}'"));
    }

    for addr in &addrs {
        let ip = addr.ip();
        // The metadata endpoint stays blocked whatever the allowlist says.
        // It is the highest-value SSRF target there is, and no legitimate
        // entry on this list resolves to it.
        if ip.to_string() == "169.254.169.254" {
            return Err("requests to cloud metadata endpoint are blocked (SSRF protection)".into());
        }
        if is_private_ip(&ip) {
            if allow_private_network {
                tracing::debug!(
                    host = %host,
                    %ip,
                    "browser profile permits private-network navigation"
                );
                continue;
            }
            if allowlisted {
                tracing::debug!(
                    host = %host,
                    %ip,
                    "host is in RUSTYKRAB_SSRF_ALLOW_HOSTS — permitting a private address"
                );
                continue;
            }
            return Err(format!("URL resolves to private IP {ip} — possible SSRF"));
        }
    }

    Ok(ValidatedUrl {
        resolved_addrs: addrs,
        host: host.to_string(),
    })
}

/// Check if an IP address is in a private/internal range.
fn is_private_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()                        // 127.0.0.0/8
                || v4.is_private()                   // 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16
                || v4.is_link_local()                // 169.254.0.0/16
                || v4.is_broadcast()                 // 255.255.255.255
                || v4.is_unspecified()               // 0.0.0.0
                || (v4.octets()[0] == 100 && v4.octets()[1] >= 64 && v4.octets()[1] <= 127)
            // 100.64.0.0/10 (CGNAT)
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()       // ::1
                || v6.is_unspecified() // ::
                // Unique local addresses (fc00::/7)
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                // Link-local addresses (fe80::/10)
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // IPv4-mapped addresses
                || v6.to_ipv4_mapped().map(|v4| {
                    v4.is_loopback() || v4.is_private() || v4.is_link_local()
                }).unwrap_or(false)
        }
    }
}

#[cfg(test)]
mod ssrf_allowlist_tests {
    use super::*;

    fn list(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// These tests never touch the environment. An earlier version set
    /// `RUSTYKRAB_SSRF_ALLOW_HOSTS` around each case, which raced
    /// `nodes::tests::list_never_leaks_tokens` -- that test carries a
    /// comment asking to remain the only one mutating env, and it was
    /// right to. Pure inputs remove the hazard instead of coordinating
    /// around it.
    #[test]
    fn an_empty_list_matches_nothing() {
        assert!(!host_matches(&[], "portal.example.com"));
    }

    #[test]
    fn matching_is_exact() {
        let allowed = list(&["portal.example.com"]);
        assert!(host_matches(&allowed, "portal.example.com"));
        assert!(
            !host_matches(&allowed, "evil-portal.example.com"),
            "a suffix is not a match"
        );
        assert!(
            !host_matches(&allowed, "example.com"),
            "a parent domain is not a match"
        );
        assert!(
            !host_matches(&allowed, "portal.example.com.evil.test"),
            "a prefix is not a match"
        );
    }

    #[test]
    fn an_empty_host_never_matches() {
        assert!(!host_matches(&list(&[""]), ""));
    }

    /// Parsing lowercases and trims, so the matcher only ever sees
    /// normalised entries; this pins that the two halves agree.
    #[test]
    fn parsing_normalises_and_drops_blanks() {
        assert!(parse_allow_hosts("").is_empty(), "unset means empty");
        assert_eq!(
            parse_allow_hosts(" A.example.com , ,b.EXAMPLE.com "),
            vec!["a.example.com".to_string(), "b.example.com".to_string()]
        );
    }

    #[test]
    fn a_tailnet_address_is_private_by_default() {
        // The range this whole feature exists for: CGNAT, which Tailscale
        // uses, and which the guard blocks unless explicitly named.
        let ip: IpAddr = "100.77.3.57".parse().unwrap();
        assert!(is_private_ip(&ip));
    }

    #[test]
    fn the_metadata_address_is_private() {
        let ip: IpAddr = "169.254.169.254".parse().unwrap();
        assert!(is_private_ip(&ip), "must never be reachable");
    }

    #[tokio::test]
    async fn an_empty_allowlist_blocks_a_tailnet_address() {
        let err = validate_url_with_allowlist("http://100.77.3.57/", &[])
            .await
            .unwrap_err();
        assert!(err.contains("private"), "{err}");
    }

    #[tokio::test]
    async fn a_named_host_may_resolve_privately() {
        assert!(
            validate_url_with_allowlist("http://100.77.3.57/", &list(&["100.77.3.57"]))
                .await
                .is_ok(),
            "an explicitly named host should be reachable"
        );
    }

    #[tokio::test]
    async fn naming_one_host_does_not_permit_another() {
        let err = validate_url_with_allowlist("http://10.0.0.5/", &list(&["100.77.3.57"]))
            .await
            .unwrap_err();
        assert!(err.contains("private"), "{err}");
    }

    /// The highest-value SSRF target there is. No entry on this list has a
    /// legitimate reason to reach it, so naming it changes nothing.
    #[tokio::test]
    async fn the_metadata_endpoint_is_never_allowlistable() {
        let err = validate_url_with_allowlist(
            "http://169.254.169.254/latest/meta-data/",
            &list(&["169.254.169.254"]),
        )
        .await
        .unwrap_err();
        assert!(err.contains("metadata"), "{err}");
    }

    #[tokio::test]
    async fn the_allowlist_never_relaxes_the_scheme_check() {
        let err = validate_url_with_allowlist("file:///etc/passwd", &list(&["evil.example.com"]))
            .await
            .unwrap_err();
        assert!(err.contains("scheme") || err.contains("host"), "{err}");
    }

    #[tokio::test]
    async fn localhost_requires_an_exact_allowlist_entry() {
        let err = validate_url_with_allowlist("http://localhost:8099/", &[])
            .await
            .unwrap_err();
        assert!(err.contains("blocked"), "{err}");

        let allowed =
            validate_url_with_allowlist("http://localhost:8099/", &list(&["localhost"])).await;
        assert!(allowed.is_ok(), "{allowed:?}");
    }
}
