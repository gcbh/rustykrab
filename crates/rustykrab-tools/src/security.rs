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

/// Validate a URL for SSRF protection.
///
/// Blocks:
/// - Private/internal IP ranges (RFC 1918, link-local, loopback)
/// - Cloud metadata endpoints (169.254.169.254)
/// - Non-HTTP(S) schemes
/// - URLs without a host
///
/// Returns resolved socket addresses to prevent DNS rebinding (TOCTOU)
/// attacks. Callers should use the returned addresses to pin connections
/// rather than re-resolving the hostname.
///
/// DNS resolution uses `tokio::net::lookup_host` to avoid blocking the
/// async runtime (fixes ASYNC-H1).
pub async fn validate_url(url: &str) -> Result<ValidatedUrl, String> {
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
        if is_private_ip(&ip) {
            return Err(format!(
                "requests to private/internal IP addresses ({ip}) are blocked (SSRF protection)"
            ));
        }
    }

    // Block known internal hostnames
    let blocked_hosts = [
        "localhost",
        "metadata.google.internal",
        "metadata.google.com",
    ];
    let host_lower = host.to_lowercase();
    for blocked in &blocked_hosts {
        if host_lower == *blocked {
            return Err(format!(
                "requests to '{host}' are blocked (SSRF protection)"
            ));
        }
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
        if is_private_ip(&ip) {
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
