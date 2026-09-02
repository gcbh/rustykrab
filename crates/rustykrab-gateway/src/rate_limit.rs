use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher, RandomState};
use std::net::IpAddr;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;

/// Number of independently locked shards. Requests from different IPs
/// hash to different shards, so contention on the hot `/api/*` path is
/// ~1/16th of a single global mutex.
const SHARD_COUNT: usize = 16;

/// Hard per-shard cap on tracked IPs (total map is bounded by
/// `SHARD_COUNT * MAX_ENTRIES_PER_SHARD` even under a unique-IP flood).
const MAX_ENTRIES_PER_SHARD: usize = 1024;

/// How often the background task sweeps stale records.
const EVICTION_INTERVAL: Duration = Duration::from_secs(30);

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

impl RateLimitConfig {
    /// Config with env-var overrides applied over the defaults:
    /// `RUSTYKRAB_RATE_LIMIT_MAX`, `RUSTYKRAB_RATE_LIMIT_WINDOW_SECS`,
    /// `RUSTYKRAB_RATE_LIMIT_LOCKOUT_SECS`.
    ///
    /// The defaults are the shipping posture; the overrides exist so a
    /// test/E2E boot can drive hundreds of requests from one IP without
    /// tripping the lockout. Unset or unparseable values keep the default.
    pub fn from_env() -> Self {
        let default = Self::default();
        // Zero and unparseable values fall back to the default: a limit of
        // 0 would reject every request, and a 0-second window would disable
        // limiting entirely — neither is ever what an operator meant.
        fn positive(key: &str) -> Option<u64> {
            let raw = std::env::var(key).ok()?;
            let raw = raw.trim();
            match raw.parse::<u64>() {
                Ok(v) if v > 0 => Some(v),
                _ => {
                    tracing::warn!(
                        var = key,
                        value = raw,
                        "ignoring invalid rate-limit override — using the default"
                    );
                    None
                }
            }
        }
        let config = Self {
            max_requests: positive("RUSTYKRAB_RATE_LIMIT_MAX")
                .and_then(|v| u32::try_from(v).ok())
                .unwrap_or(default.max_requests),
            window: positive("RUSTYKRAB_RATE_LIMIT_WINDOW_SECS")
                .map(Duration::from_secs)
                .unwrap_or(default.window),
            lockout: positive("RUSTYKRAB_RATE_LIMIT_LOCKOUT_SECS")
                .map(Duration::from_secs)
                .unwrap_or(default.lockout),
        };
        if config.max_requests != default.max_requests
            || config.window != default.window
            || config.lockout != default.lockout
        {
            tracing::warn!(
                max_requests = config.max_requests,
                window_secs = config.window.as_secs(),
                lockout_secs = config.lockout.as_secs(),
                "rate limiter running with non-default limits from the environment"
            );
        }
        config
    }
}

struct IpRecord {
    attempts: Vec<Instant>,
    locked_until: Option<Instant>,
}

impl IpRecord {
    /// A record is stale when its lockout (if any) has expired and it has
    /// seen no attempts since `stale_cutoff`.
    fn is_stale(&self, now: Instant, stale_cutoff: Instant) -> bool {
        let locked_expired = self.locked_until.map(|l| l <= now).unwrap_or(true);
        let no_recent = self
            .attempts
            .last()
            .map(|&t| t <= stale_cutoff)
            .unwrap_or(true);
        locked_expired && no_recent
    }
}

type Shard = Mutex<HashMap<IpAddr, IpRecord>>;

/// In-memory, per-IP rate limiter.
///
/// Prevents brute-force attacks (CVE-2026-32025 class) by tracking
/// request counts per source IP with an automatic lockout.
///
/// Records are striped across [`SHARD_COUNT`] mutexed shards keyed by IP
/// hash. Stale records are swept by a periodic background task (when a
/// tokio runtime is available) and each shard is hard-capped at
/// [`MAX_ENTRIES_PER_SHARD`] so memory stays bounded under unique-IP
/// floods.
pub struct RateLimiter {
    config: RateLimitConfig,
    shards: Arc<[Shard; SHARD_COUNT]>,
    hasher: RandomState,
}

impl RateLimiter {
    pub fn new(config: RateLimitConfig) -> Self {
        let shards: Arc<[Shard; SHARD_COUNT]> =
            Arc::new(std::array::from_fn(|_| Mutex::new(HashMap::new())));

        // Periodic stale-record sweep. Holds only a Weak reference so the
        // task exits once the limiter is dropped. Skipped when constructed
        // outside a tokio runtime (e.g. in sync tests) — the per-shard hard
        // cap still bounds memory in that case.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let weak: Weak<[Shard; SHARD_COUNT]> = Arc::downgrade(&shards);
            let window = config.window;
            handle.spawn(async move {
                let mut ticker = tokio::time::interval(EVICTION_INTERVAL);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    ticker.tick().await;
                    let Some(shards) = weak.upgrade() else { break };
                    let now = Instant::now();
                    let stale_cutoff = now - window * 2;
                    for shard in shards.iter() {
                        let mut map = shard.lock().unwrap_or_else(|e| e.into_inner());
                        map.retain(|_, rec| !rec.is_stale(now, stale_cutoff));
                    }
                }
            });
        }

        Self {
            config,
            shards,
            hasher: RandomState::new(),
        }
    }

    fn shard_for(&self, ip: IpAddr) -> &Shard {
        let mut hasher = self.hasher.build_hasher();
        match ip {
            IpAddr::V4(v4) => hasher.write(&v4.octets()),
            IpAddr::V6(v6) => hasher.write(&v6.octets()),
        }
        &self.shards[(hasher.finish() as usize) % SHARD_COUNT]
    }

    /// Returns `true` if the request should be allowed.
    pub fn check(&self, ip: IpAddr) -> bool {
        let mut records = self.shard_for(ip).lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();

        // Enforce the hard cap before inserting a new IP so a unique-IP
        // flood can't grow the shard unboundedly between sweeps.
        if !records.contains_key(&ip) && records.len() >= MAX_ENTRIES_PER_SHARD {
            Self::evict_one(&mut records, now, now - self.config.window * 2);
        }

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

        true
    }

    /// Make room in a full shard: drop a stale record if one exists,
    /// otherwise the record with the oldest last attempt that isn't
    /// currently locked out (so active lockouts are never reset), and as
    /// a last resort the oldest record outright.
    fn evict_one(records: &mut HashMap<IpAddr, IpRecord>, now: Instant, stale_cutoff: Instant) {
        let mut oldest_unlocked: Option<(IpAddr, Instant)> = None;
        let mut oldest_any: Option<(IpAddr, Instant)> = None;
        for (ip, rec) in records.iter() {
            if rec.is_stale(now, stale_cutoff) {
                let ip = *ip;
                records.remove(&ip);
                return;
            }
            let last = rec.attempts.last().copied().unwrap_or(now);
            if oldest_any.map(|(_, t)| last < t).unwrap_or(true) {
                oldest_any = Some((*ip, last));
            }
            let locked = rec.locked_until.map(|l| l > now).unwrap_or(false);
            if !locked && oldest_unlocked.map(|(_, t)| last < t).unwrap_or(true) {
                oldest_unlocked = Some((*ip, last));
            }
        }
        if let Some((ip, _)) = oldest_unlocked.or(oldest_any) {
            records.remove(&ip);
        }
    }

    /// Total number of tracked IP records across all shards.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|s| s.lock().unwrap_or_else(|e| e.into_inner()).len())
            .sum()
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

    // A request carrying a token this gateway accepts is not what this
    // limiter defends against. Its purpose is brute-force and
    // unauthenticated abuse (CVE-2026-32025); a caller that already holds
    // the secret has full agent access on this machine, up to and
    // including shell execution, so counting its requests protects
    // nothing.
    //
    // It does break legitimate use. Peer delegation is asynchronous, so
    // collecting a result means polling `/api/tasks/{id}` — which exhausts
    // a 20-request window in well under a minute and then earns a
    // five-minute lockout. Behind `tailscale serve` it is worse: the proxy
    // connects from loopback, so every delegated request and the node's
    // own WebChat share one bucket.
    //
    // Guesses, which is what brute force actually looks like, do not match
    // and are still counted and locked out below.
    // Lifted out of the request before the await: holding a `&Request`
    // across one makes the middleware future non-Send, since the body
    // type is not Sync.
    let bearer = bearer_token(request.headers());

    if is_authenticated(&state, bearer.as_deref()).await {
        return Ok(next.run(request).await);
    }

    let client_ip = extract_client_ip(&request, addr.ip());

    if !state.rate_limiter.check(client_ip) {
        tracing::warn!(ip = %client_ip, path = %path, "rate limited");
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }

    Ok(next.run(request).await)
}

/// Pull the bearer credential out of an `Authorization` header.
///
/// Returns `None` for anything that is not a non-empty `Bearer` value, so
/// a malformed or empty header can never be mistaken for a credential and
/// is rate-limited like any other anonymous request.
fn bearer_token(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
}

/// Whether the request carries a bearer token this gateway accepts.
///
/// Deliberately mirrors [`crate::auth::require_auth`]'s notion of a valid
/// token — master or per-device — so the two middlewares cannot disagree
/// about who is authenticated. A token that fails both checks falls
/// through to the limiter, so an attacker cannot buy an exemption by
/// attaching an `Authorization` header to a guess.
async fn is_authenticated(state: &crate::AppState, token: Option<&str>) -> bool {
    let Some(token) = token else {
        return false;
    };

    // Held across the comparison to avoid a TOCTOU race with rotation,
    // and reduced to a bool so the guard cannot live across the await
    // below — the same discipline `require_auth` uses.
    let is_master = {
        let guard = state.auth_token.read().unwrap_or_else(|e| e.into_inner());
        rustykrab_core::crypto::constant_time_eq(token, &guard)
    };
    if is_master {
        return true;
    }

    matches!(
        state.agent.store.devices().authenticate(token).await,
        Ok(Some(_))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn config(max_requests: u32) -> RateLimitConfig {
        RateLimitConfig {
            max_requests,
            window: Duration::from_secs(60),
            lockout: Duration::from_secs(300),
        }
    }

    fn ip(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    /// Build an Authorization header map.
    fn headers(value: &str) -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_str(value).unwrap(),
        );
        h
    }

    #[test]
    fn a_bearer_credential_is_recognised() {
        assert_eq!(
            bearer_token(&headers("Bearer abc123")).as_deref(),
            Some("abc123")
        );
    }

    #[test]
    fn anonymous_and_malformed_requests_stay_rate_limited() {
        // Each of these must yield no credential, so the caller falls
        // through to the counter. An attacker must not be able to buy an
        // exemption just by attaching an Authorization header.
        assert_eq!(bearer_token(&axum::http::HeaderMap::new()), None);
        assert_eq!(bearer_token(&headers("Bearer ")), None);
        assert_eq!(bearer_token(&headers("Bearer    ")), None);
        assert_eq!(bearer_token(&headers("Basic abc123")), None);
        assert_eq!(bearer_token(&headers("abc123")), None);
        // Case matters: the scheme is "Bearer", and a near-miss is not a
        // credential.
        assert_eq!(bearer_token(&headers("bearer abc123")), None);
    }

    #[test]
    fn a_guess_is_not_a_credential() {
        // The exemption turns on the token *matching*, not merely being
        // present — brute force is exactly a stream of well-formed
        // guesses, and those must still be counted and locked out.
        let master = "the-real-token";
        let guess = bearer_token(&headers("Bearer not-the-real-token")).unwrap();
        assert!(!rustykrab_core::crypto::constant_time_eq(&guess, master));

        let real = bearer_token(&headers("Bearer the-real-token")).unwrap();
        assert!(rustykrab_core::crypto::constant_time_eq(&real, master));
    }

    #[test]
    fn the_limiter_still_locks_out_unauthenticated_floods() {
        // The exemption is in the middleware, not the limiter: the
        // counter itself is unchanged, so anonymous traffic behaves
        // exactly as before.
        let limiter = RateLimiter::new(config(3));
        let client = ip(10, 0, 0, 7);
        for _ in 0..3 {
            assert!(limiter.check(client));
        }
        assert!(
            !limiter.check(client),
            "fourth request must trip the lockout"
        );
        assert!(!limiter.check(client), "and stay locked out");
    }

    #[test]
    fn allows_up_to_limit_then_locks_out() {
        let limiter = RateLimiter::new(config(3));
        let client = ip(10, 0, 0, 1);
        for _ in 0..3 {
            assert!(limiter.check(client));
        }
        // Fourth request trips the lockout; subsequent requests stay blocked.
        assert!(!limiter.check(client));
        assert!(!limiter.check(client));
    }

    #[test]
    fn ips_are_tracked_independently() {
        let limiter = RateLimiter::new(config(1));
        assert!(limiter.check(ip(10, 0, 0, 1)));
        assert!(!limiter.check(ip(10, 0, 0, 1)));
        // A different IP (any shard) is unaffected.
        assert!(limiter.check(ip(10, 0, 99, 2)));
    }

    #[test]
    fn unique_ip_flood_is_bounded_by_hard_cap() {
        let limiter = RateLimiter::new(config(20));
        // Far more unique IPs than the total capacity.
        for a in 0..80u32 {
            for b in 0..256u32 {
                assert!(limiter.check(ip(1, (a % 256) as u8, (b / 256) as u8, b as u8)));
            }
        }
        assert!(limiter.len() <= SHARD_COUNT * MAX_ENTRIES_PER_SHARD);
    }

    #[test]
    fn eviction_prefers_stale_and_preserves_lockouts() {
        let now = Instant::now();
        let mut records: HashMap<IpAddr, IpRecord> = HashMap::new();
        records.insert(
            ip(1, 1, 1, 1),
            IpRecord {
                attempts: vec![now],
                locked_until: Some(now + Duration::from_secs(300)),
            },
        );
        records.insert(
            ip(2, 2, 2, 2),
            IpRecord {
                attempts: vec![now],
                locked_until: None,
            },
        );
        // Locked-out record survives; the unlocked one is evicted.
        RateLimiter::evict_one(&mut records, now, now - Duration::from_secs(120));
        assert!(records.contains_key(&ip(1, 1, 1, 1)));
        assert!(!records.contains_key(&ip(2, 2, 2, 2)));
    }

    #[tokio::test]
    async fn background_sweep_removes_stale_records() {
        let limiter = RateLimiter::new(RateLimitConfig {
            max_requests: 5,
            window: Duration::from_millis(1),
            lockout: Duration::from_millis(1),
        });
        assert!(limiter.check(ip(10, 0, 0, 7)));
        assert!(limiter.len() >= 1);
        // Wait until the record is stale, then run one sweep by hand
        // (the real task ticks every 30s — too slow for a test).
        tokio::time::sleep(Duration::from_millis(5)).await;
        let now = Instant::now();
        for shard in limiter.shards.iter() {
            let mut map = shard.lock().unwrap();
            map.retain(|_, rec| !rec.is_stale(now, now - Duration::from_millis(2)));
        }
        assert_eq!(limiter.len(), 0);
    }
}
