use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;

/// Configuration for the rate limiter.
#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    /// Maximum number of requests per window.
    pub max_requests: u32,
    /// Duration of the sliding window.
    pub window: Duration,
    /// Duration to lock out an IP after exceeding the limit.
    pub lockout: Duration,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            max_requests: 20,
            window: Duration::from_secs(60),
            lockout: Duration::from_secs(300), // 5-minute lockout
        }
    }
}

struct IpRecord {
    attempts: Vec<Instant>,
    locked_until: Option<Instant>,
}

/// In-memory, per-IP rate limiter.
///
/// Prevents brute-force attacks (CVE-2026-32025 class) by tracking
/// request counts per source IP with an automatic lockout.
pub struct RateLimiter {
    config: RateLimitConfig,
    records: Mutex<HashMap<IpAddr, IpRecord>>,
}

impl RateLimiter {
    pub fn new(config: RateLimitConfig) -> Self {
        Self {
            config,
            records: Mutex::new(HashMap::new()),
        }
    }

    /// Returns `true` if the request should be allowed.
    pub fn check(&self, ip: IpAddr) -> bool {
        let mut records = self.records.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();

        let record = records.entry(ip).or_insert_with(|| IpRecord {
            attempts: Vec::new(),
            locked_until: None,
        });

        // Check lockout.
        if let Some(until) = record.locked_until {
            if now < until {
                return false;
            }
            // Lockout expired — reset.
            record.locked_until = None;
            record.attempts.clear();
        }

        // Prune old attempts outside the window.
        let cutoff = now - self.config.window;
        record.attempts.retain(|t| *t > cutoff);

        // Check limit.
        if record.attempts.len() >= self.config.max_requests as usize {
            record.locked_until = Some(now + self.config.lockout);
            tracing::warn!(%ip, "rate limit exceeded, locking out");
            return false;
        }

        record.attempts.push(now);

        // Prune stale entries to prevent unbounded memory growth.
        // Only scan a bounded number of entries per call to avoid
        // iterating the entire HashMap under the lock during DDoS.
        if records.len() > 1_000 {
            let stale_cutoff = now - self.config.window * 2;
            let stale_keys: Vec<IpAddr> = records
                .iter()
                .take(512) // Bound scan to 512 entries per request.
                .filter(|(_, rec)| {
                    // Stale if lockout expired (or never locked).
                    let locked_expired = rec.locked_until.map(|l| l <= now).unwrap_or(true);
                    // Stale if no recent activity.
                    let no_recent = rec
                        .attempts
                        .last()
                        .map(|&t| t <= stale_cutoff)
                        .unwrap_or(true);
                    locked_expired && no_recent
                })
                .map(|(ip, _)| *ip)
                .take(256)
                .collect();
            for key in &stale_keys {
                records.remove(key);
            }
        }

        true
    }
}

/// Extract the client IP address, preferring the X-Forwarded-For header
/// when the request arrives through a reverse proxy.
fn extract_client_ip(request: &Request, socket_addr: IpAddr) -> IpAddr {
    // Only trust X-Forwarded-For when the direct connection is from
    // loopback (i.e. a local reverse proxy). This prevents spoofing
    // by external clients injecting the header directly.
    if socket_addr.is_loopback() {
        if let Some(forwarded) = request.headers().get("x-forwarded-for") {
            if let Ok(value) = forwarded.to_str() {
                // X-Forwarded-For: client, proxy1, proxy2
                // The leftmost entry is the original client IP.
                if let Some(client_ip) = value.split(',').next() {
                    if let Ok(ip) = client_ip.trim().parse::<IpAddr>() {
                        return ip;
                    }
                }
            }
        }
    }
    socket_addr
}

/// Axum middleware that enforces rate limiting on API endpoints.
pub async fn rate_limit_middleware(
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    state: axum::extract::State<crate::AppState>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let path = request.uri().path();

    // Only rate-limit API endpoints.
    if !path.starts_with("/api/") {
        return Ok(next.run(request).await);
    }

    let client_ip = extract_client_ip(&request, addr.ip());

    if !state.rate_limiter.check(client_ip) {
        tracing::warn!(ip = %client_ip, path = %path, "rate limited");
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }

    Ok(next.run(request).await)
}
