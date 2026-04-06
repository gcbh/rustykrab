//! Shared security utilities for tool implementations.
//!
//! Provides path traversal prevention and SSRF protection that are
//! reused across multiple tool implementations.

use std::net::IpAddr;
use std::path::{Component, PathBuf};

/// Default safe base directory for file operations.
/// If OPENCLAW_WORKSPACE environment variable is set, it is used instead.
pub fn workspace_root() -> PathBuf {
    std::env::var("OPENCLAW_WORKSPACE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/tmp/openclaw"))
        })
}

/// Validate that a file path is safe and within allowed boundaries.
///
/// Returns the canonicalized path if valid, or an error message.
/// Prevents:
/// - Path traversal via `..` components
/// - Symlink escapes (by canonicalizing)
/// - Access to sensitive system directories
pub fn validate_path(path: &str) -> Result<PathBuf, String> {
    let path_buf = PathBuf::from(path);

    // Reject paths with .. components before canonicalization
    for component in path_buf.components() {
        if matches!(component, Component::ParentDir) {
            return Err("path traversal (.. components) is not allowed".into());
        }
    }

    // Sensitive directories that should never be accessed
    let blocked_prefixes: &[&str] = &[
        "/etc/shadow",
        "/etc/sudoers",
        "/root/.ssh",
        "/proc",
        "/sys",
        "/dev",
    ];

    let path_str = path_buf.to_string_lossy();
    for prefix in blocked_prefixes {
        if path_str.starts_with(prefix) {
            return Err(format!("access to {prefix} is blocked for security reasons"));
        }
    }

    // For existing files, canonicalize to resolve symlinks and verify location
    if path_buf.exists() {
        let canonical = path_buf.canonicalize().map_err(|e| {
            format!("failed to resolve path: {e}")
        })?;

        let canonical_str = canonical.to_string_lossy();
        for prefix in blocked_prefixes {
            if canonical_str.starts_with(prefix) {
                return Err(format!(
                    "resolved path points to blocked location: {prefix}"
                ));
            }
        }

        return Ok(canonical);
    }

    // For new files, validate the parent exists and is safe
    if let Some(parent) = path_buf.parent() {
        if parent.exists() {
            let canonical_parent = parent.canonicalize().map_err(|e| {
                format!("failed to resolve parent directory: {e}")
            })?;

            let canonical_str = canonical_parent.to_string_lossy();
            for prefix in blocked_prefixes {
                if canonical_str.starts_with(prefix) {
                    return Err(format!(
                        "parent directory resolves to blocked location: {prefix}"
                    ));
                }
            }
        }
    }

    Ok(path_buf)
}

/// Validate a URL for SSRF protection.
///
/// Blocks:
/// - Private/internal IP ranges (RFC 1918, link-local, loopback)
/// - Cloud metadata endpoints (169.254.169.254)
/// - Non-HTTP(S) schemes
/// - URLs without a host
pub fn validate_url(url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(url)
        .map_err(|e| format!("invalid URL: {e}"))?;

    // Only allow http and https schemes
    match parsed.scheme() {
        "http" | "https" => {}
        other => return Err(format!("URL scheme '{other}' is not allowed (only http/https)")),
    }

    let host = parsed.host_str()
        .ok_or("URL must have a host")?;

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
            return Err(format!("requests to '{host}' are blocked (SSRF protection)"));
        }
    }

    // Block 169.254.169.254 (AWS/GCP metadata) even as hostname
    if host == "169.254.169.254" {
        return Err("requests to cloud metadata endpoint are blocked (SSRF protection)".into());
    }

    // Resolve hostname and check all IPs against private ranges to prevent DNS rebinding
    let port = parsed.port().unwrap_or(if parsed.scheme() == "https" { 443 } else { 80 });
    if let Ok(addrs) = std::net::ToSocketAddrs::to_socket_addrs(&(host, port)) {
        for addr in addrs {
            let ip = addr.ip();
            if is_private_ip(&ip) {
                return Err(format!(
                    "URL resolves to private IP {ip} — possible SSRF"
                ));
            }
        }
    }

    Ok(())
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
                || v4.octets()[0] == 100 && v4.octets()[1] >= 64 && v4.octets()[1] <= 127  // 100.64.0.0/10 (CGNAT)
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()       // ::1
                || v6.is_unspecified() // ::
                // IPv4-mapped addresses
                || v6.to_ipv4_mapped().map(|v4| {
                    v4.is_loopback() || v4.is_private() || v4.is_link_local()
                }).unwrap_or(false)
        }
    }
}
