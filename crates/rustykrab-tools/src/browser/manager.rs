//! Multi-profile browser manager modeled after OpenClaw's architecture.
//!
//! Manages multiple isolated browser instances, each with its own CDP port,
//! user-data directory, and lifecycle. The manager handles:
//! - Launching/connecting to Chrome instances per profile
//! - Tab management (list, open, close, focus) by stable Chrome target ID
//! - Browser lifecycle (start, stop, status)
//! - Chrome profile symlink setup for cookie/session persistence
//! - Process tracking (Child handles) so spawned Chromes can be killed
//! - Health checks before reuse so dead browsers are auto-replaced
//! - Best-effort kill of all spawned children on Drop

use chromiumoxide::cdp::browser_protocol::network::EventRequestWillBeSentExtraInfo;
use chromiumoxide::handler::HandlerConfig;
use chromiumoxide::Browser;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_stream::StreamExt;

use super::config::{BrowserConfig, DriverType};
use super::downloads::DownloadTracker;
use rustykrab_core::{Error, Result};

/// How long to wait for a launched Chrome to start serving CDP before giving
/// up. Generous so cold launches on slow disks still succeed.
const POST_LAUNCH_TIMEOUT_MS: u64 = 15_000;

/// Cap on the per-probe HTTP timeout when polling `/json/version`.
const HEALTH_PROBE_TIMEOUT_MS: u64 = 3_000;

/// Cap on a single `Page::url()` probe. `url()` is a CDP round trip, and an
/// unbounded one inherits the client's own 30s fallback — long enough that a
/// tool call is killed by its budget and retried, three times over, before
/// the browser ever answers.
const PAGE_PROBE_TIMEOUT_MS: u64 = 2_000;

/// Cap on the *total* spent probing pages while resolving which tab to use.
/// Bounds the whole loop, not each call, so a profile at its tab cap cannot
/// multiply the per-probe budget by the number of tabs.
const RESOLVE_PROBE_BUDGET_MS: u64 = 5_000;

/// Stealth-oriented Chrome launch flags. These reduce the most obvious
/// "this is a headed automation browser" signals — automation banner,
/// `navigator.webdriver`, and the timer throttling that causes the macOS
/// "Chrome went to sleep" pattern.
const STEALTH_LAUNCH_ARGS: &[&str] = &[
    "--disable-blink-features=AutomationControlled",
    "--disable-features=IsolateOrigins,site-per-process",
    "--window-size=1920,1080",
    "--lang=en-US",
    "--disable-background-timer-throttling",
    "--disable-backgrounding-occluded-windows",
    "--disable-renderer-backgrounding",
    "--no-default-browser-check",
    "--disable-infobars",
    "--disable-default-apps",
];

/// State for a single browser profile instance.
pub struct ProfileInstance {
    pub browser: Browser,
    pub _handler_task: tokio::task::JoinHandle<()>,
    #[allow(dead_code)]
    pub profile_name: String,
    pub cdp_url: String,
    pub launched_by_us: bool,
    protocol_health: Arc<ProtocolHealth>,
    pub download_tracker: Option<DownloadTracker>,
    pub _download_task: Option<tokio::task::JoinHandle<()>>,
    pub download_setup_error: Option<String>,
}

impl ProfileInstance {
    fn abort_background_tasks(&self) {
        self._handler_task.abort();
        if let Some(task) = &self._download_task {
            task.abort();
        }
    }
}

/// Observable health of the long-lived CDP event stream.
///
/// Chromiumoxide normally swallows events it cannot decode and emits a log
/// line with neither the CDP method nor a counter. Surfacing them lets status
/// and incident logs distinguish an alive Chrome process from a protocol
/// client that is steadily losing browser state.
#[derive(Debug, Default)]
struct ProtocolHealth {
    invalid_messages: AtomicU64,
    tolerated_messages: AtomicU64,
    handler_exited: AtomicBool,
    invalid_methods: std::sync::Mutex<HashMap<String, u64>>,
    tolerated_methods: std::sync::Mutex<HashMap<String, u64>>,
    last_invalid_error: std::sync::Mutex<Option<String>>,
    last_tolerated_reason: std::sync::Mutex<Option<String>>,
}

impl ProtocolHealth {
    fn record_invalid_method(&self, method: String, error: String) -> u64 {
        let count = self.invalid_messages.fetch_add(1, Ordering::Relaxed) + 1;
        let mut methods = match self.invalid_methods.lock() {
            Ok(methods) => methods,
            Err(poisoned) => poisoned.into_inner(),
        };
        *methods.entry(method).or_default() += 1;
        let mut last_error = match self.last_invalid_error.lock() {
            Ok(error) => error,
            Err(poisoned) => poisoned.into_inner(),
        };
        *last_error = Some(error);
        count
    }

    fn last_invalid_error(&self) -> Option<String> {
        match self.last_invalid_error.lock() {
            Ok(error) => error.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn invalid_method_summary(&self) -> Vec<serde_json::Value> {
        method_summary(&self.invalid_methods)
    }

    fn record_tolerated_method(&self, method: String, reason: String) -> u64 {
        let count = self.tolerated_messages.fetch_add(1, Ordering::Relaxed) + 1;
        let mut methods = match self.tolerated_methods.lock() {
            Ok(methods) => methods,
            Err(poisoned) => poisoned.into_inner(),
        };
        *methods.entry(method).or_default() += 1;
        let mut last_reason = match self.last_tolerated_reason.lock() {
            Ok(reason) => reason,
            Err(poisoned) => poisoned.into_inner(),
        };
        *last_reason = Some(reason);
        count
    }

    fn tolerated_method_summary(&self) -> Vec<serde_json::Value> {
        method_summary(&self.tolerated_methods)
    }

    fn last_tolerated_reason(&self) -> Option<String> {
        match self.last_tolerated_reason.lock() {
            Ok(reason) => reason.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

fn method_summary(methods: &std::sync::Mutex<HashMap<String, u64>>) -> Vec<serde_json::Value> {
    let methods = match methods.lock() {
        Ok(methods) => methods,
        Err(poisoned) => poisoned.into_inner(),
    };
    let mut entries: Vec<_> = methods.iter().collect();
    entries.sort_by(|(method_a, count_a), (method_b, count_b)| {
        count_b.cmp(count_a).then_with(|| method_a.cmp(method_b))
    });
    entries
        .into_iter()
        .take(16)
        .map(|(method, count)| serde_json::json!({ "method": method, "count": count }))
        .collect()
}

/// Manages multiple browser profiles, each an isolated Chrome instance.
pub struct BrowserManager {
    config: BrowserConfig,
    /// Active browser instances keyed by profile name.
    instances: Arc<Mutex<HashMap<String, ProfileInstance>>>,
    /// Child process handles for browsers we launched ourselves.
    /// Stored separately under a `std::sync::Mutex` so the `Drop` impl
    /// (synchronous) can kill them even while the async `instances` lock
    /// is held elsewhere.
    children: Arc<std::sync::Mutex<HashMap<String, std::process::Child>>>,
    /// The tab each session last navigated, keyed by `(session, profile)`.
    ///
    /// Without this, an action that omits `targetId` re-resolves the page
    /// from scratch every call, and one browser profile is shared by every
    /// concurrent run. Pinning the navigated tab is what keeps a `navigate`
    /// and the `snapshot` after it on the same page.
    sticky: Arc<std::sync::Mutex<HashMap<(String, String), String>>>,
    /// One browser profile is one mutable Chrome process. Serialize tool calls
    /// that address it so a recovery, navigation, or tab mutation cannot race
    /// another conversation using the same profile.
    action_leases: Arc<std::sync::Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

/// How many tabs one profile's browser may hold.
///
/// The browser is deliberately long-lived: it keeps the operator's
/// logged-in profile warm and survives daemon restarts, which is what
/// gets an agent past bot protection without signing in again. The cost
/// of that choice is that nothing ever closes a tab -- the agent opens
/// them and rarely tidies up, so a browser that lives for weeks
/// accumulates them until it is the largest process on the machine.
///
/// A cap keeps reuse constant-cost.
///
/// It deliberately does not claim to close the *oldest* tab. There is no
/// age to sort by: `Browser::pages()` collects from a `HashMap` with
/// `values_mut()`, and the handler permutes that map on every poll
/// (`remove_entry` + `insert`, and `swap_remove` on its id list), so the
/// order is arbitrary and unstable between calls. Chrome's target IDs are
/// opaque and carry no age either. An earlier version drained the front of
/// that list and described it as oldest-first; it closed an arbitrary
/// subset.
///
/// So reap by what can actually be known: blank startup tabs first, which
/// are always safe, and never a tab some session has pinned. One profile
/// is shared by every concurrent run, so closing a pinned tab would take a
/// page out from under an agent mid-task -- recoverable, since
/// `resolve_target` clears a dead pin and falls back, but the agent
/// silently loses the page it was working on. If every tab is pinned,
/// exceed the cap rather than break someone's session.
///
/// Closing is best-effort: failing to tidy must never fail the navigation
/// the user actually asked for.
const MAX_TABS_PER_PROFILE: usize = 8;

impl BrowserManager {
    pub fn new(config: BrowserConfig) -> Self {
        let mgr = Self {
            config,
            instances: Arc::new(Mutex::new(HashMap::new())),
            children: Arc::new(std::sync::Mutex::new(HashMap::new())),
            sticky: Arc::new(std::sync::Mutex::new(HashMap::new())),
            action_leases: Arc::new(std::sync::Mutex::new(HashMap::new())),
        };
        if std::env::var("RUSTYKRAB_BROWSER_SWEEP").as_deref() == Ok("1") {
            sweep_stale_processes();
        }
        mgr
    }

    /// Load configuration and create a manager.
    pub fn from_config() -> Self {
        Self::new(BrowserConfig::load())
    }

    pub fn config(&self) -> &BrowserConfig {
        &self.config
    }

    /// Acquire exclusive access to one mutable browser profile for a tool call.
    pub async fn acquire_profile_lease(
        &self,
        profile_name: &str,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        let lease = {
            let mut leases = match self.action_leases.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            Arc::clone(
                leases
                    .entry(profile_name.to_string())
                    .or_insert_with(|| Arc::new(Mutex::new(()))),
            )
        };
        lease.lock_owned().await
    }

    /// Get or create a browser instance for the given profile.
    ///
    /// Performs a liveness check on any cached instance: a dead Chrome
    /// (process killed, OS crashed it, CDP stopped responding) is evicted
    /// and replaced with a fresh launch.
    pub async fn get_browser(
        &self,
        profile_name: &str,
    ) -> Result<Arc<Mutex<HashMap<String, ProfileInstance>>>> {
        let mut instances = self.instances.lock().await;

        let needs_relaunch = match instances.get(profile_name) {
            Some(inst) => !is_instance_alive(inst).await,
            None => true,
        };

        if needs_relaunch {
            if let Some(dead) = instances.remove(profile_name) {
                tracing::warn!(
                    profile = profile_name,
                    "browser instance failed health check — relaunching"
                );
                dead.abort_background_tasks();
                self.kill_child(profile_name);
                drop(dead.browser);
            }
            let instance = self.connect_or_launch(profile_name).await?;
            instances.insert(profile_name.to_string(), instance);
        }

        drop(instances);
        Ok(Arc::clone(&self.instances))
    }

    /// Check the status of a profile's browser.
    pub async fn status(&self, profile_name: &str) -> serde_json::Value {
        let instances = self.instances.lock().await;
        if let Some(inst) = instances.get(profile_name) {
            let alive = is_instance_alive(inst).await;
            let page_count = inst.browser.pages().await.map(|p| p.len()).unwrap_or(0);
            let invalid_messages = inst
                .protocol_health
                .invalid_messages
                .load(Ordering::Relaxed);
            let tolerated_messages = inst
                .protocol_health
                .tolerated_messages
                .load(Ordering::Relaxed);
            let handler_running = !inst.protocol_health.handler_exited.load(Ordering::Acquire);
            let browser_version = tokio::time::timeout(
                Duration::from_millis(HEALTH_PROBE_TIMEOUT_MS),
                inst.browser.version(),
            )
            .await
            .ok()
            .and_then(std::result::Result::ok);
            let protocol_invalid_methods = inst.protocol_health.invalid_method_summary();
            let protocol_last_invalid_error = inst.protocol_health.last_invalid_error();
            let protocol_tolerated_methods = inst.protocol_health.tolerated_method_summary();
            let protocol_last_tolerated_reason = inst.protocol_health.last_tolerated_reason();
            let download_dir = inst
                .download_tracker
                .as_ref()
                .map(|tracker| tracker.root().display().to_string());
            serde_json::json!({
                "status": if alive { "running" } else { "unresponsive" },
                "profile": profile_name,
                "cdp_url": inst.cdp_url,
                "launched_by_us": inst.launched_by_us,
                "protocol_status": if !handler_running {
                    "disconnected"
                } else if invalid_messages > 0 {
                    "compatibility_warning"
                } else if tolerated_messages > 0 {
                    "healthy_with_tolerated_drift"
                } else {
                    "healthy"
                },
                "protocol_invalid_messages": invalid_messages,
                "protocol_invalid_methods": protocol_invalid_methods,
                "protocol_last_invalid_error": protocol_last_invalid_error,
                "protocol_tolerated_messages": tolerated_messages,
                "protocol_tolerated_methods": protocol_tolerated_methods,
                "protocol_last_tolerated_reason": protocol_last_tolerated_reason,
                "protocol_handler_running": handler_running,
                "browser_product": browser_version.as_ref().map(|version| version.product.clone()),
                "browser_protocol_version": browser_version.as_ref().map(|version| version.protocol_version.clone()),
                "downloads_status": if inst.download_tracker.is_some() { "ready" } else { "unavailable" },
                "download_dir": download_dir,
                "download_setup_error": inst.download_setup_error,
                "tabs": page_count
            })
        } else {
            let cdp_url = self.config.resolve_cdp_url(profile_name);
            let probe_timeout = Duration::from_millis(self.health_probe_timeout_ms());
            let reachable = probe_cdp(&cdp_url, probe_timeout).await;
            serde_json::json!({
                "status": if reachable { "available" } else { "stopped" },
                "profile": profile_name,
                "cdp_url": cdp_url,
                "launched_by_us": false,
                "tabs": 0
            })
        }
    }

    /// Sequence marker used to correlate a click with browser-level download
    /// events that begin after it.
    pub async fn download_sequence(&self, profile_name: &str) -> Option<u64> {
        let tracker = {
            let instances = self.instances.lock().await;
            instances
                .get(profile_name)
                .and_then(|instance| instance.download_tracker.clone())
        };
        match tracker {
            Some(tracker) => Some(tracker.sequence().await),
            None => None,
        }
    }

    pub async fn list_downloads(&self, profile_name: &str) -> Result<serde_json::Value> {
        let tracker = {
            let instances = self.instances.lock().await;
            instances
                .get(profile_name)
                .and_then(|instance| instance.download_tracker.clone())
        }
        .ok_or_else(|| {
            Error::ToolExecution(
                format!("download observation is unavailable for profile '{profile_name}'").into(),
            )
        })?;
        Ok(serde_json::json!({
            "status": "ok",
            "profile": profile_name,
            "download_dir": tracker.root().display().to_string(),
            "downloads": tracker.list().await,
        }))
    }

    pub async fn observe_downloads(
        &self,
        profile_name: &str,
        since: u64,
        budget: Duration,
    ) -> Option<serde_json::Value> {
        let tracker = {
            let instances = self.instances.lock().await;
            instances
                .get(profile_name)
                .and_then(|instance| instance.download_tracker.clone())
        }?;
        Some(tracker.observe_since(since, budget).await)
    }

    /// Start a browser for the given profile (if not already running).
    pub async fn start(&self, profile_name: &str) -> Result<serde_json::Value> {
        let mut instances = self.instances.lock().await;
        if let Some(inst) = instances.get(profile_name) {
            if is_instance_alive(inst).await {
                return Ok(serde_json::json!({
                    "status": "already_running",
                    "profile": profile_name
                }));
            }
            // Stale entry — fall through to relaunch.
            tracing::warn!(
                profile = profile_name,
                "existing browser entry is unresponsive — replacing"
            );
            if let Some(dead) = instances.remove(profile_name) {
                dead.abort_background_tasks();
                drop(dead.browser);
            }
            self.kill_child(profile_name);
        }

        let instance = self.connect_or_launch(profile_name).await?;
        let cdp_url = instance.cdp_url.clone();
        instances.insert(profile_name.to_string(), instance);

        Ok(serde_json::json!({
            "status": "started",
            "profile": profile_name,
            "cdp_url": cdp_url
        }))
    }

    /// Stop a browser for the given profile.
    pub async fn stop(&self, profile_name: &str) -> Result<serde_json::Value> {
        let mut instances = self.instances.lock().await;
        if let Some(inst) = instances.remove(profile_name) {
            inst.abort_background_tasks();
            drop(inst.browser);
            let killed = self.kill_child(profile_name);
            Ok(serde_json::json!({
                "status": "stopped",
                "profile": profile_name,
                "process_killed": killed,
            }))
        } else {
            Ok(serde_json::json!({
                "status": "not_running",
                "profile": profile_name
            }))
        }
    }

    /// Drop a degraded CDP client and establish a fresh one.
    ///
    /// Managed Chrome is restarted because a target/renderer timeout can leave
    /// the process responsive at `Browser.getVersion` while its page session is
    /// unusable. Attach-only/remote profiles are never killed; only their CDP
    /// client and target sessions are rebuilt.
    pub async fn recover_profile(&self, profile_name: &str) -> Result<serde_json::Value> {
        let mut instances = self.instances.lock().await;
        let old = instances.remove(profile_name);
        let was_running = old.is_some();
        if let Some(old) = old {
            old.abort_background_tasks();
            drop(old.browser);
        }

        let process_restarted = self.kill_child(profile_name);
        self.clear_profile_sticky_targets(profile_name);

        tracing::warn!(
            profile = profile_name,
            was_running,
            process_restarted,
            "recovering degraded browser profile"
        );

        let replacement = self.connect_or_launch(profile_name).await?;
        let cdp_url = replacement.cdp_url.clone();
        let launched_by_us = replacement.launched_by_us;
        instances.insert(profile_name.to_string(), replacement);

        Ok(serde_json::json!({
            "status": "recovered",
            "profile": profile_name,
            "cdp_url": cdp_url,
            "process_restarted": process_restarted,
            "launched_by_us": launched_by_us
        }))
    }

    /// Stop every running profile. Best-effort; errors are logged.
    ///
    /// This is the async, graceful counterpart to `Drop` (which only kills
    /// child processes synchronously). Wire it into your shutdown sequence
    /// if you want clean CDP disconnects before exit.
    #[allow(dead_code)]
    pub async fn shutdown_all(&self) {
        let names: Vec<String> = {
            let instances = self.instances.lock().await;
            instances.keys().cloned().collect()
        };
        for name in names {
            if let Err(e) = self.stop(&name).await {
                tracing::warn!(profile = %name, error = %e, "failed to stop browser during shutdown");
            }
        }
    }

    /// List all known profiles and their status.
    pub async fn profiles(&self) -> serde_json::Value {
        let instances = self.instances.lock().await;
        let profiles: Vec<serde_json::Value> = self
            .config
            .profiles
            .keys()
            .map(|name| {
                let running = instances.contains_key(name);
                serde_json::json!({
                    "name": name,
                    "running": running,
                    "cdp_url": self.config.resolve_cdp_url(name),
                    "driver": format!("{:?}", self.config.profiles.get(name).map(|p| &p.driver).unwrap_or(&DriverType::Rustykrab)),
                })
            })
            .collect();
        serde_json::json!({ "profiles": profiles })
    }

    /// List tabs for a profile's browser.
    pub async fn tabs(&self, profile_name: &str) -> Result<serde_json::Value> {
        let instances = self.instances.lock().await;
        let inst = instances.get(profile_name).ok_or_else(|| {
            Error::ToolExecution(
                format!(
                    "browser not running for profile '{profile_name}'. Use action 'start' first."
                )
                .into(),
            )
        })?;

        let pages = inst
            .browser
            .pages()
            .await
            .map_err(|e| Error::ToolExecution(format!("failed to list tabs: {e}").into()))?;

        // Address tabs by Chrome's own target ID. `pages()` walks a
        // `HashMap`, so position in that list is arbitrary and unstable
        // between calls — a positional `tab_N` names a different page from
        // one call to the next.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(RESOLVE_PROBE_BUDGET_MS);
        let mut rows = Vec::new();
        for page in pages.iter() {
            // One unresponsive tab should still leave the listing usable, so
            // it is reported with what could be read rather than blocking it.
            let url = probe_page_url(page, deadline).await.unwrap_or_default();
            let title = probe_page_title(page, deadline).await.unwrap_or_default();
            rows.push((page.target_id().inner().clone(), url, title));
        }
        // Sort so the listing itself is stable across calls; `pages()` is not.
        rows.sort_by(|a, b| a.0.cmp(&b.0));

        let tabs: Vec<serde_json::Value> = rows
            .into_iter()
            .map(|(target_id, url, title)| {
                serde_json::json!({
                    "targetId": target_id,
                    "url": url,
                    "title": title,
                })
            })
            .collect();

        Ok(serde_json::json!({
            "tabs": tabs,
            "count": tabs.len(),
            "profile": profile_name
        }))
    }

    /// Open a new tab with the given URL.
    pub async fn open_tab(&self, profile_name: &str, url: &str) -> Result<serde_json::Value> {
        let instances = self.instances.lock().await;
        let inst = instances.get(profile_name).ok_or_else(|| {
            Error::ToolExecution(format!("browser not running for profile '{profile_name}'").into())
        })?;

        let nav_timeout = Duration::from_millis(self.config.remote_cdp_timeout_ms.max(10_000));
        let open_deadline = tokio::time::Instant::now() + nav_timeout;

        // `new_page` creates the tab *and* navigates, so this budget covers
        // a page load, not just a CDP round trip. Timing the two phases
        // separately is the difference between "the browser is wedged" and
        // "the site is slow", which the old single error could not tell
        // apart.
        // Keep reuse bounded before adding to it.
        // `Page::close` consumes self (CDP `Target.closeTarget`), so the
        // stale pages are taken by value rather than borrowed -- same
        // reason `close_tab` uses `swap_remove`.
        if let Ok(existing) = inst.browser.pages().await {
            if existing.len() >= MAX_TABS_PER_PROFILE {
                let wanted = existing.len() + 1 - MAX_TABS_PER_PROFILE;
                let pinned = self.pinned_targets(profile_name);

                // Rank what may go: blank startup tabs first, then other
                // unpinned tabs. Pinned tabs are never candidates.
                let mut blank = Vec::new();
                let mut other = Vec::new();
                let deadline =
                    tokio::time::Instant::now() + Duration::from_millis(RESOLVE_PROBE_BUDGET_MS);
                for page in existing {
                    if pinned.contains(page.target_id().inner()) {
                        continue;
                    }
                    // Bounded, and deliberately the opposite polarity to
                    // `get_page`: there an unreadable tab is ranked with the
                    // blank ones so it cannot win, here it is ranked away
                    // from them so it does not get closed. Both directions
                    // are "when in doubt, leave it alone".
                    let blank_page = probe_page_url(&page, deadline)
                        .await
                        .as_deref()
                        .map(is_blank_url)
                        .unwrap_or(false);
                    if blank_page {
                        blank.push(page);
                    } else {
                        other.push(page);
                    }
                }
                blank.extend(other);

                let mut closed = 0;
                for stale in blank.into_iter().take(wanted) {
                    match stale.close().await {
                        Ok(()) => closed += 1,
                        // A tab that will not close is not a reason to
                        // refuse the navigation the user asked for.
                        Err(e) => tracing::warn!(error = %e, "could not close a stale tab"),
                    }
                }
                tracing::info!(
                    closed,
                    wanted,
                    pinned = pinned.len(),
                    cap = MAX_TABS_PER_PROFILE,
                    "reaped tabs before opening a new one"
                );
                if closed < wanted {
                    // Over the cap because the rest are in use. Say so:
                    // silently exceeding a documented limit is the kind of
                    // thing that reads as a leak later.
                    tracing::debug!(
                        short_by = wanted - closed,
                        "kept tabs that sessions are using; over the cap for now"
                    );
                }
            }
        }

        let open_started = std::time::Instant::now();
        let page = tokio::time::timeout_at(open_deadline, inst.browser.new_page(url))
            .await
            .map_err(|_| {
                tracing::warn!(
                    url,
                    budget_ms = nav_timeout.as_millis() as u64,
                    "new_page timed out — tab creation plus navigation exceeded the budget"
                );
                Error::ToolExecution(
                    format!(
                        "open_tab timed out after {}ms waiting for the tab to be created \
                         and '{url}' to load. The browser was reachable (the instance is \
                         alive); the page did not finish in the budget.",
                        nav_timeout.as_millis()
                    )
                    .into(),
                )
            })?
            .map_err(|e| Error::ToolExecution(format!("failed to open tab: {e}").into()))?;
        let open_ms = open_started.elapsed().as_millis() as u64;

        // Tab creation and settling share one absolute deadline. Two separate
        // full-size timeouts made a slow open cost twice what the caller asked
        // for and could collide with the runner's outer tool timeout.
        let nav_started = std::time::Instant::now();
        let settled = if tokio::time::Instant::now() < open_deadline {
            tokio::time::timeout_at(open_deadline, page.wait_for_navigation())
                .await
                .is_ok()
        } else {
            false
        };
        tracing::info!(
            url,
            open_ms,
            settle_ms = nav_started.elapsed().as_millis() as u64,
            settled,
            "opened tab"
        );
        let actual_url = probe_page_url_once(&page).await.unwrap_or_default();
        let title = probe_page_title_once(&page).await.unwrap_or_default();

        Ok(serde_json::json!({
            "status": "opened",
            "url": actual_url,
            "title": title,
            "targetId": page.target_id().inner().clone(),
            "profile": profile_name
        }))
    }

    /// Close a tab by index.
    pub async fn close_tab(
        &self,
        profile_name: &str,
        target_id: &str,
    ) -> Result<serde_json::Value> {
        let instances = self.instances.lock().await;
        let inst = instances.get(profile_name).ok_or_else(|| {
            Error::ToolExecution(format!("browser not running for profile '{profile_name}'").into())
        })?;

        let mut pages = inst
            .browser
            .pages()
            .await
            .map_err(|e| Error::ToolExecution(format!("failed to list tabs: {e}").into()))?;

        let idx = find_target_index(&pages, target_id)?;

        // `Page::close` (the CDP `Target.closeTarget`) consumes self, so we
        // take the page by value out of the Vec. This avoids `window.close()`
        // which Chrome blocks for tabs not opened via script.
        let page = pages.swap_remove(idx);
        page.close()
            .await
            .map_err(|e| Error::ToolExecution(format!("close_tab failed: {e}").into()))?;

        Ok(serde_json::json!({
            "status": "closed",
            "targetId": target_id,
            "profile": profile_name
        }))
    }

    /// Focus (bring to front) a tab by targetId.
    pub async fn focus_tab(
        &self,
        profile_name: &str,
        target_id: &str,
    ) -> Result<serde_json::Value> {
        let instances = self.instances.lock().await;
        let inst = instances.get(profile_name).ok_or_else(|| {
            Error::ToolExecution(format!("browser not running for profile '{profile_name}'").into())
        })?;

        let pages = inst
            .browser
            .pages()
            .await
            .map_err(|e| Error::ToolExecution(format!("failed to list tabs: {e}").into()))?;

        let page = &pages[find_target_index(&pages, target_id)?];
        page.bring_to_front()
            .await
            .map_err(|e| Error::ToolExecution(format!("failed to focus tab: {e}").into()))?;

        let url = probe_page_url_once(page).await.unwrap_or_default();
        let title = probe_page_title_once(page).await.unwrap_or_default();

        Ok(serde_json::json!({
            "status": "focused",
            "targetId": target_id,
            "url": url,
            "title": title,
            "profile": profile_name
        }))
    }

    /// Pin the tab this session is working in, so later actions that omit
    /// `targetId` address the same page.
    pub fn set_sticky_target(&self, session: &str, profile: &str, target_id: &str) {
        let key = (session.to_string(), profile.to_string());
        let mut sticky = match self.sticky.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        sticky.insert(key, target_id.to_string());
    }

    /// The tab this session last navigated, if any.
    pub fn sticky_target(&self, session: &str, profile: &str) -> Option<String> {
        let key = (session.to_string(), profile.to_string());
        let sticky = match self.sticky.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        sticky.get(&key).cloned()
    }

    /// Every target pinned by any session for this profile.
    ///
    /// The reap needs "is anyone using this tab", not "is *this* session
    /// using it": the profile is shared, so another conversation's pinned
    /// page is exactly what must not be closed.
    pub fn pinned_targets(&self, profile: &str) -> std::collections::HashSet<String> {
        let sticky = match self.sticky.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        sticky
            .iter()
            .filter(|((_, p), _)| p == profile)
            .map(|(_, target)| target.clone())
            .collect()
    }

    /// Forget this session's pinned tab — called when it turns out to be
    /// gone, so the next action falls back to normal resolution instead of
    /// failing forever on a closed target.
    pub fn clear_sticky_target(&self, session: &str, profile: &str) {
        let key = (session.to_string(), profile.to_string());
        let mut sticky = match self.sticky.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        sticky.remove(&key);
    }

    fn clear_profile_sticky_targets(&self, profile: &str) {
        let mut sticky = match self.sticky.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        sticky.retain(|(_, pinned_profile), _| pinned_profile != profile);
    }

    /// Whether `target_id` still names a live tab in this profile.
    ///
    /// `Some(false)` only when the browser answered and the tab was absent.
    /// `None` means the probe could not tell — a browser that is merely slow
    /// must not cost a session the tab it is working in, so callers keep the
    /// pin on `None` and only drop it on a definite answer.
    pub async fn target_is_live(&self, profile_name: &str, target_id: &str) -> Option<bool> {
        let instances = self.instances.lock().await;
        // No instance is a definite answer, not an inconclusive one: a
        // profile with no browser has no tabs, so a pin naming one is dead.
        // This is the path a pin takes across a Chrome relaunch.
        let Some(inst) = instances.get(profile_name) else {
            return Some(false);
        };
        let listing = tokio::time::timeout(
            Duration::from_millis(HEALTH_PROBE_TIMEOUT_MS),
            inst.browser.pages(),
        )
        .await;
        match listing {
            Ok(Ok(pages)) => Some(pages.iter().any(|p| p.target_id().inner() == target_id)),
            // Timed out, or the handler is gone: inconclusive.
            _ => None,
        }
    }

    /// Get a specific page by targetId, or the session's current page.
    pub async fn get_page(
        &self,
        profile_name: &str,
        target_id: Option<&str>,
    ) -> Result<chromiumoxide::Page> {
        let instances = self.instances.lock().await;
        let inst = instances.get(profile_name).ok_or_else(|| {
            Error::ToolExecution(
                format!(
                    "browser not running for profile '{profile_name}'. Use action 'start' first."
                )
                .into(),
            )
        })?;

        let pages = inst
            .browser
            .pages()
            .await
            .map_err(|e| Error::ToolExecution(format!("failed to list pages: {e}").into()))?;

        if let Some(tid) = target_id {
            let idx = find_target_index(&pages, tid)?;
            return Ok(pages.into_iter().nth(idx).unwrap());
        }

        // No explicit target. `pages()` walks a `HashMap`, so `pages[0]` is
        // an arbitrary tab that can differ between two consecutive calls —
        // that is how a `navigate` and the `snapshot` after it end up on
        // different pages, one of them the permanent `about:blank` startup
        // tab. Rank instead: real pages before blank ones, ties broken on
        // target ID, so repeated calls agree on the same page.
        //
        // Probing costs a CDP round trip per tab, so it runs under a shared
        // deadline: a tab that will not answer ranks alongside the blank ones
        // rather than stalling resolution or winning by default.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(RESOLVE_PROBE_BUDGET_MS);
        let mut ranked: Vec<(bool, String, usize)> = Vec::with_capacity(pages.len());
        for (i, page) in pages.iter().enumerate() {
            let blank = probe_page_url(page, deadline)
                .await
                .as_deref()
                .map(is_blank_url)
                .unwrap_or(true);
            ranked.push((blank, page.target_id().inner().clone(), i));
        }
        ranked.sort();

        if let Some((_, _, idx)) = ranked.into_iter().next() {
            return Ok(pages.into_iter().nth(idx).unwrap());
        }

        // No pages — create one.
        inst.browser
            .new_page("about:blank")
            .await
            .map_err(|e| Error::ToolExecution(format!("failed to create tab: {e}").into()))
    }

    /// Connect to an existing browser or launch a new one for the given profile.
    async fn connect_or_launch(&self, profile_name: &str) -> Result<ProfileInstance> {
        let cdp_url = self.config.resolve_cdp_url(profile_name);
        let attach_only = self.config.is_attach_only(profile_name);
        let connect_timeout = Duration::from_millis(self.config.remote_cdp_timeout_ms);

        // Try connecting to an existing instance first
        match tokio::time::timeout(connect_timeout, self.connect_browser(&cdp_url)).await {
            Ok(Ok((browser, handler))) => {
                let (handler_task, protocol_health) =
                    spawn_handler_task(handler, profile_name.to_string());
                let (download_tracker, download_task, download_setup_error) =
                    self.setup_download_tracker(&browser, profile_name).await;
                return Ok(ProfileInstance {
                    browser,
                    _handler_task: handler_task,
                    profile_name: profile_name.to_string(),
                    cdp_url,
                    launched_by_us: false,
                    protocol_health,
                    download_tracker,
                    _download_task: download_task,
                    download_setup_error,
                });
            }
            Ok(Err(e)) => {
                if attach_only {
                    return Err(Error::ToolExecution(
                        format!("cannot connect to browser at {cdp_url} (attach-only mode): {e}")
                            .into(),
                    ));
                }
                tracing::info!(
                    profile = profile_name,
                    "browser not reachable at {cdp_url}, launching..."
                );
            }
            Err(_) => {
                if attach_only {
                    return Err(Error::ToolExecution(
                        format!(
                            "timed out connecting to browser at {cdp_url} \
                             after {}ms (attach-only mode)",
                            connect_timeout.as_millis()
                        )
                        .into(),
                    ));
                }
                tracing::info!(
                    profile = profile_name,
                    "browser CDP connect at {cdp_url} timed out after {}ms, launching...",
                    connect_timeout.as_millis()
                );
            }
        }

        // Launch a new browser instance via spawn_blocking to avoid
        // blocking the async runtime with std::fs and std::process
        // operations (fixes ASYNC-H3).
        let config = self.config.clone();
        let profile = profile_name.to_string();
        let child = tokio::task::spawn_blocking(move || launch_browser_blocking(&config, &profile))
            .await
            .map_err(|e| Error::ToolExecution(format!("launch task failed: {e}").into()))??;

        // Track the child PID so `stop()` and Drop can kill it.
        if let Ok(mut children) = self.children.lock() {
            // If an old entry exists (somehow), drop it.
            if let Some(mut old) = children.insert(profile_name.to_string(), child) {
                let _ = old.kill();
                let _ = old.wait();
            }
        }

        // Wait for the freshly-launched Chrome to start serving CDP, instead
        // of the old fixed 2-second sleep. Poll `/json/version` until we get
        // a 200, with a generous overall budget.
        let probe_timeout = Duration::from_millis(self.health_probe_timeout_ms());
        let launch_deadline =
            tokio::time::Instant::now() + Duration::from_millis(POST_LAUNCH_TIMEOUT_MS);
        let mut became_ready = false;
        while tokio::time::Instant::now() < launch_deadline {
            if probe_cdp(&cdp_url, probe_timeout).await {
                became_ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        if !became_ready {
            // Kill the child we just launched — it's not serving CDP.
            self.kill_child(profile_name);
            return Err(Error::ToolExecution(
                format!(
                    "browser launched but CDP never came up at {cdp_url} \
                     within {POST_LAUNCH_TIMEOUT_MS}ms"
                )
                .into(),
            ));
        }

        let (browser, handler) =
            match tokio::time::timeout(connect_timeout, self.connect_browser(&cdp_url)).await {
                Ok(Ok(pair)) => pair,
                Ok(Err(e)) => {
                    self.kill_child(profile_name);
                    return Err(Error::ToolExecution(
                        format!(
                            "browser not reachable at {cdp_url} after launch attempt: {e}. \
                         If a browser is already running without remote debugging, \
                         quit it first so a new instance can start."
                        )
                        .into(),
                    ));
                }
                Err(_) => {
                    self.kill_child(profile_name);
                    return Err(Error::ToolExecution(
                        format!(
                            "timed out connecting to browser at {cdp_url} \
                         after {}ms (post-launch). \
                         If a browser is already running without remote debugging, \
                         quit it first so a new instance can start.",
                            connect_timeout.as_millis()
                        )
                        .into(),
                    ));
                }
            };

        let (handler_task, protocol_health) = spawn_handler_task(handler, profile_name.to_string());
        let (download_tracker, download_task, download_setup_error) =
            self.setup_download_tracker(&browser, profile_name).await;

        Ok(ProfileInstance {
            browser,
            _handler_task: handler_task,
            profile_name: profile_name.to_string(),
            cdp_url,
            launched_by_us: true,
            protocol_health,
            download_tracker,
            _download_task: download_task,
            download_setup_error,
        })
    }

    async fn connect_browser(
        &self,
        cdp_url: &str,
    ) -> chromiumoxide::Result<(Browser, chromiumoxide::Handler)> {
        let config = HandlerConfig {
            // Surface rejected events to our handler task. It records only the
            // method name and parse error, never the event payload.
            ignore_invalid_messages: false,
            request_timeout: Duration::from_millis(
                self.config.cdp_request_timeout_ms.clamp(500, 10_000),
            ),
            ..HandlerConfig::default()
        };
        Browser::connect_with_config(cdp_url, config).await
    }

    fn health_probe_timeout_ms(&self) -> u64 {
        self.config
            .remote_cdp_timeout_ms
            .min(HEALTH_PROBE_TIMEOUT_MS)
    }

    async fn setup_download_tracker(
        &self,
        browser: &Browser,
        profile_name: &str,
    ) -> (
        Option<DownloadTracker>,
        Option<tokio::task::JoinHandle<()>>,
        Option<String>,
    ) {
        if self.config.is_remote_profile(profile_name) {
            return (
                None,
                None,
                Some(
                    "remote downloads require an artifact-transfer channel; local path verification is disabled"
                        .to_string(),
                ),
            );
        }
        setup_download_tracker(
            browser,
            self.config.resolve_download_dir(profile_name),
            profile_name,
        )
        .await
    }

    /// Kill the stored Child for the given profile. Returns true if a
    /// process was killed.
    fn kill_child(&self, profile_name: &str) -> bool {
        let Ok(mut children) = self.children.lock() else {
            return false;
        };
        let Some(mut child) = children.remove(profile_name) else {
            return false;
        };
        let pid = child.id();
        match child.kill() {
            Ok(()) => {
                let _ = child.wait();
                tracing::info!(profile = profile_name, pid, "killed browser child");
                true
            }
            Err(e) => {
                tracing::warn!(
                    profile = profile_name,
                    pid,
                    "failed to kill browser child: {e}"
                );
                false
            }
        }
    }
}

impl Drop for BrowserManager {
    fn drop(&mut self) {
        // Best-effort: kill every Chrome we launched. We may be running
        // outside any Tokio context here, so this stays synchronous and
        // does not touch the async `instances` lock.
        let Ok(mut children) = self.children.lock() else {
            return;
        };
        for (name, mut child) in children.drain() {
            let pid = child.id();
            if let Err(e) = child.kill() {
                tracing::warn!(
                    profile = %name,
                    pid,
                    "failed to kill browser child during drop: {e}"
                );
                continue;
            }
            let _ = child.wait();
            tracing::info!(profile = %name, pid, "killed browser child during drop");
        }
    }
}

/// Spawn the CDP event handler. The task drains events until the underlying
/// connection closes, then logs the exit so an unexpected disconnect is
/// visible in the logs.
fn spawn_handler_task(
    mut handler: chromiumoxide::Handler,
    profile_name: String,
) -> (tokio::task::JoinHandle<()>, Arc<ProtocolHealth>) {
    let health = Arc::new(ProtocolHealth::default());
    let task_health = Arc::clone(&health);
    let task = tokio::spawn(async move {
        while let Some(event) = handler.next().await {
            match event {
                Err(chromiumoxide::error::CdpError::InvalidMessage(raw, error)) => {
                    let parsed = serde_json::from_str::<serde_json::Value>(&raw).ok();
                    let method = parsed
                        .as_ref()
                        .and_then(|message| message["method"].as_str().map(str::to_string))
                        .unwrap_or_else(|| "unknown".to_string());
                    let diagnostic = if method == EventRequestWillBeSentExtraInfo::IDENTIFIER {
                        parsed
                            .as_ref()
                            .and_then(|message| message.get("params").cloned())
                            .and_then(|params| {
                                serde_json::from_value::<EventRequestWillBeSentExtraInfo>(params)
                                    .err()
                                    .map(|parse_error| parse_error.to_string())
                            })
                            .unwrap_or_else(|| error.to_string())
                    } else {
                        error.to_string()
                    };
                    if let Some(reason) = known_tolerated_protocol_drift(&method, parsed.as_ref()) {
                        let count =
                            task_health.record_tolerated_method(method.clone(), reason.to_string());
                        if count <= 3 || count.is_power_of_two() {
                            tracing::debug!(
                                profile = %profile_name,
                                method,
                                count,
                                reason,
                                "tolerated known CDP schema drift for an unconsumed event"
                            );
                        }
                        continue;
                    }
                    let count = task_health.record_invalid_method(method.clone(), diagnostic);
                    // Avoid reproducing chromiumoxide's warning flood. The first
                    // few events and powers of two preserve timing and scale.
                    if count <= 3 || count.is_power_of_two() {
                        tracing::warn!(
                            profile = %profile_name,
                            method,
                            count,
                            error = %error,
                            "CDP event rejected by protocol decoder"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(profile = %profile_name, error = %e, "CDP handler event error");
                }
                Ok(_) => {}
            }
        }
        task_health.handler_exited.store(true, Ordering::Release);
        tracing::info!(profile = %profile_name, "browser CDP handler task exited");
    });
    (task, health)
}

/// Chromiumoxide 0.9.1 is generated from a Chrome 142-era protocol and eagerly
/// deserializes every event, even those RustyKrab never subscribes to. Chrome
/// 152 renamed one required field in `ClientSecurityState`; rejecting this
/// request metadata does not affect navigation, DOM, action, or download state.
/// Keep this exception exact so any other protocol drift remains an alert.
fn known_tolerated_protocol_drift(
    method: &str,
    message: Option<&serde_json::Value>,
) -> Option<&'static str> {
    if method != EventRequestWillBeSentExtraInfo::IDENTIFIER {
        return None;
    }
    let security_state = message?
        .get("params")?
        .get("clientSecurityState")?
        .as_object()?;
    if security_state.contains_key("localNetworkAccessRequestPolicy")
        && !security_state.contains_key("privateNetworkRequestPolicy")
    {
        Some(
            "Chrome 152 renamed ClientSecurityState.privateNetworkRequestPolicy to localNetworkAccessRequestPolicy; this unconsumed request metadata event was dropped",
        )
    } else {
        None
    }
}

async fn setup_download_tracker(
    browser: &Browser,
    root: std::path::PathBuf,
    profile_name: &str,
) -> (
    Option<DownloadTracker>,
    Option<tokio::task::JoinHandle<()>>,
    Option<String>,
) {
    match tokio::time::timeout(
        Duration::from_secs(4),
        DownloadTracker::attach(browser, root),
    )
    .await
    {
        Ok(Ok((tracker, task))) => (Some(tracker), Some(task), None),
        Ok(Err(error)) => {
            let message = error.to_string();
            tracing::warn!(
                profile = profile_name,
                error = %message,
                "browser connected without download observation"
            );
            (None, None, Some(message))
        }
        Err(_) => {
            let message = "download observer setup timed out after 4s".to_string();
            tracing::warn!(
                profile = profile_name,
                "browser connected without download observation: {message}"
            );
            (None, None, Some(message))
        }
    }
}

/// Health check for a cached `ProfileInstance`. We do a cheap CDP
/// round-trip (`Browser.getVersion`) with a short timeout. A failure
/// (timeout or CDP error) means the browser has gone away.
async fn is_instance_alive(inst: &ProfileInstance) -> bool {
    if inst._handler_task.is_finished()
        || inst.protocol_health.handler_exited.load(Ordering::Acquire)
    {
        return false;
    }
    let probe_timeout = Duration::from_millis(HEALTH_PROBE_TIMEOUT_MS);
    matches!(
        tokio::time::timeout(probe_timeout, inst.browser.version()).await,
        Ok(Ok(_))
    )
}

/// Launch a Chrome/Chromium instance for the given profile and return the
/// owning `Child`. The free-function shape keeps it `spawn_blocking`-safe.
fn launch_browser_blocking(
    config: &BrowserConfig,
    profile_name: &str,
) -> Result<std::process::Child> {
    let cdp_url = config.resolve_cdp_url(profile_name);
    let port = cdp_url
        .rsplit(':')
        .next()
        .and_then(|p| p.trim_end_matches('/').parse::<u16>().ok())
        .unwrap_or(18800);

    let user_data_dir = config.resolve_user_data_dir(profile_name);
    std::fs::create_dir_all(&user_data_dir)
        .map_err(|e| Error::ToolExecution(format!("failed to create user-data dir: {e}").into()))?;

    // Set up profile symlink for cookie persistence (managed profiles only)
    let profile = config.profiles.get(profile_name);
    let driver = profile.map(|p| &p.driver).unwrap_or(&DriverType::Rustykrab);
    let profile_dir_name = if *driver == DriverType::Rustykrab {
        setup_profile_link(
            &user_data_dir,
            !should_borrow_system_profile(config, profile_name),
        )
    } else {
        "Default".to_string()
    };

    let mut args: Vec<String> = vec![
        format!("--remote-debugging-port={port}"),
        format!("--user-data-dir={}", user_data_dir.display()),
        format!("--profile-directory={profile_dir_name}"),
        "--no-first-run".to_string(),
    ];

    if config.is_headless(profile_name) {
        args.push("--headless=new".to_string());
    }
    if config.is_no_sandbox(profile_name) {
        args.push("--no-sandbox".to_string());
        args.push("--disable-setuid-sandbox".to_string());
    }

    // Stealth + reliability flags. These are appended *before* user-supplied
    // extra_args so explicit overrides win.
    for flag in STEALTH_LAUNCH_ARGS {
        args.push((*flag).to_string());
    }

    // Extra args from config
    args.extend(config.extra_args.iter().cloned());

    args.push("about:blank".to_string());

    // Resolve executable
    let executable = config
        .resolve_executable(profile_name)
        .or_else(detect_chrome_executable);

    let exe = executable.ok_or_else(|| {
        Error::ToolExecution(
            "no supported browser found (Chrome/Brave/Edge/Chromium). \
             Install Google Chrome or set CHROME_EXECUTABLE / executablePath."
                .into(),
        )
    })?;

    // Chrome's own stderr, kept next to its profile.
    //
    // It used to go to /dev/null, which meant a browser that failed to
    // start, crashed, or refused a profile lock said exactly nothing --
    // the only visible symptom was a tool timeout further up, with no way
    // to tell "never launched" from "launched and slow". Diagnosing
    // `open_tab timed out` from outside the process is guesswork without
    // this.
    let chrome_log_path = user_data_dir.join("chrome-stderr.log");
    let chrome_log = std::fs::File::create(&chrome_log_path).ok();

    let mut cmd = std::process::Command::new(&exe);
    cmd.args(&args);
    match chrome_log {
        Some(f) => {
            let dup = f.try_clone().ok();
            cmd.stdout(std::process::Stdio::from(f));
            match dup {
                Some(d) => {
                    cmd.stderr(std::process::Stdio::from(d));
                }
                None => {
                    cmd.stderr(std::process::Stdio::null());
                }
            }
        }
        None => {
            cmd.stdout(std::process::Stdio::null());
            cmd.stderr(std::process::Stdio::null());
        }
    }

    // macOS: prevent App Nap from suspending the headed browser's timers.
    // Headed Chrome backgrounded on macOS will throttle its event loop and
    // CDP can become unresponsive — disabling AppSleep keeps it lively.
    #[cfg(target_os = "macos")]
    {
        cmd.env("NSAppSleepDisabled", "YES");
    }

    let child = cmd.spawn().map_err(|e| {
        Error::ToolExecution(format!("failed to launch browser ({exe}): {e}").into())
    })?;

    tracing::info!(
        port,
        profile = profile_name,
        %profile_dir_name,
        pid = child.id(),
        executable = %exe,
        user_data_dir = %user_data_dir.display(),
        stderr_log = %chrome_log_path.display(),
        "launched browser with remote debugging"
    );
    Ok(child)
}

fn should_borrow_system_profile(config: &BrowserConfig, profile_name: &str) -> bool {
    config.isolated_root.is_none()
        && config
            .profiles
            .get(profile_name)
            .and_then(|profile| profile.user_data_dir.as_ref())
            .is_none()
}

/// Best-effort kill of any Chromium-like process whose user-data-dir lives
/// under `~/.rustykrab/browser/`. Opt-in via `RUSTYKRAB_BROWSER_SWEEP=1` —
/// we won't touch unrelated browser processes, but startup sweeps are still
/// destructive enough that they should be explicit.
fn sweep_stale_processes() {
    #[cfg(unix)]
    {
        // Match the user-data-dir we always pass on the command line.
        let pattern = ".rustykrab/browser/";
        match std::process::Command::new("pkill")
            .args(["-f", &format!("user-data-dir=.*{pattern}")])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
        {
            Ok(s) if s.success() => {
                tracing::info!(pattern, "swept stale rustykrab browser processes");
            }
            Ok(_) => {
                // pkill exit code 1 == no matches found, which is fine.
            }
            Err(e) => {
                tracing::warn!("startup sweep skipped: pkill failed: {e}");
            }
        }
    }
    #[cfg(not(unix))]
    {
        // Not implemented on non-unix.
    }
}

/// Detect the platform-specific Chrome data directory.
fn chrome_data_dir() -> Option<std::path::PathBuf> {
    let home = std::env::var("HOME").ok()?;
    #[cfg(target_os = "macos")]
    {
        Some(std::path::PathBuf::from(home).join("Library/Application Support/Google/Chrome"))
    }
    #[cfg(target_os = "linux")]
    {
        Some(std::path::PathBuf::from(home).join(".config/google-chrome"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// Read Chrome's `Local State` to find the last-used profile directory.
fn detect_profile_name(chrome_dir: &std::path::Path) -> String {
    let local_state_path = chrome_dir.join("Local State");
    if let Ok(data) = std::fs::read_to_string(&local_state_path) {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&data) {
            if let Some(name) = parsed["profile"]["last_used"].as_str() {
                if chrome_dir.join(name).exists() {
                    return name.to_string();
                }
            }
        }
    }
    "Default".to_string()
}

/// Set up a wrapper data directory that symlinks back to the user's real
/// Chrome profile, preserving cookies and sessions.
/// Link the account's real Chrome profile into `user_data_dir`, unless
/// the caller asked for an isolated browser.
///
/// Borrowing the real profile is the point in normal use -- it carries
/// the logins. Under an isolated root it is precisely what the caller
/// asked to avoid, and in an eval it silently invalidates the result: a
/// trial that inherits a signed-in cookie looks like a successful
/// sign-in without ever having signed in.
///
/// An explicit per-profile user-data directory is also isolated: the caller
/// named the exact data root, so silently linking the account profile into it
/// would violate that boundary (and made live tests mutate the real profile).
///
/// `isolated` is passed in rather than read from the environment here.
/// One setting read in one place is easier to reason about than the same
/// `env::var` in two functions, and it makes this decision testable
/// without mutating process-global state -- which in this workspace is
/// shared with every other test in the binary.
fn setup_profile_link(user_data_dir: &std::path::Path, isolated: bool) -> String {
    let Some(chrome_dir) = chrome_data_dir().filter(|_| !isolated) else {
        write_local_state(user_data_dir, "Default");
        return "Default".to_string();
    };

    let profile_name = detect_profile_name(&chrome_dir);
    let real_profile = chrome_dir.join(&profile_name);
    let link_path = user_data_dir.join(&profile_name);

    if real_profile.exists() && !link_path.exists() {
        #[cfg(unix)]
        {
            if let Err(e) = std::os::unix::fs::symlink(&real_profile, &link_path) {
                tracing::warn!("could not symlink Chrome profile: {e}");
            }
        }
    }

    write_local_state(user_data_dir, &profile_name);

    profile_name
}

/// Minimal `Local State`, which keeps Chrome's profile picker from
/// appearing. A picker means no page, and no page means every action
/// times out with nothing to say why.
fn write_local_state(user_data_dir: &std::path::Path, profile_name: &str) {
    let local_state = serde_json::json!({
        "profile": {
            "last_used": profile_name,
            "last_active_profiles": [profile_name],
            "picker_shown": false
        }
    });
    if let Err(e) = std::fs::write(user_data_dir.join("Local State"), local_state.to_string()) {
        tracing::warn!("could not write Chrome Local State: {e}");
    }
}

/// Detect the Chrome/Chromium executable path for the current platform.
fn detect_chrome_executable() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let candidates = [
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
        ];
        for path in &candidates {
            if std::path::Path::new(path).exists() {
                return Some(path.to_string());
            }
        }
        None
    }
    #[cfg(target_os = "linux")]
    {
        let candidates = [
            "google-chrome",
            "google-chrome-stable",
            "chromium-browser",
            "chromium",
            "brave-browser",
            "microsoft-edge",
        ];
        for name in &candidates {
            if std::process::Command::new("which")
                .arg(name)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
            {
                return Some(name.to_string());
            }
        }
        None
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// Probe a CDP URL to check if a browser is reachable, with a timeout.
async fn probe_cdp(url: &str, timeout: Duration) -> bool {
    let version_url = format!("{}/json/version", url.trim_end_matches('/'));
    let client = match reqwest::Client::builder().timeout(timeout).build() {
        Ok(c) => c,
        Err(_) => return false,
    };
    matches!(
        client.get(&version_url).send().await,
        Ok(r) if r.status().is_success()
    )
}

/// Read a page's URL under a bound, returning `None` when it will not answer.
///
/// Measured: `url()` is served from the handler's cached target info, so it
/// returns in microseconds even against a spinning renderer. The bound is
/// insurance for the case the handler itself is saturated, not a fix for a
/// hang — [`probe_page_title`] is where the real exposure is.
///
/// Callers treat `None` as "cannot tell", never as a fact about the page.
pub(super) async fn probe_page_url(
    page: &chromiumoxide::Page,
    deadline: tokio::time::Instant,
) -> Option<String> {
    let now = tokio::time::Instant::now();
    if now >= deadline {
        return None;
    }
    let budget = Duration::from_millis(PAGE_PROBE_TIMEOUT_MS).min(deadline - now);
    match tokio::time::timeout(budget, page.url()).await {
        Ok(Ok(url)) => url,
        _ => None,
    }
}

/// Read a page's title under a bound, returning `None` when it will not answer.
///
/// Unlike [`probe_page_url`], this is a real round trip to the renderer, so a
/// page spinning in JavaScript never answers it. Measured against a tab in a
/// `while(true)` loop, an unbounded call had still not returned after 40s —
/// long enough to blow a tool-call budget and be retried on top.
pub(super) async fn probe_page_title(
    page: &chromiumoxide::Page,
    deadline: tokio::time::Instant,
) -> Option<String> {
    let now = tokio::time::Instant::now();
    if now >= deadline {
        return None;
    }
    let budget = Duration::from_millis(PAGE_PROBE_TIMEOUT_MS).min(deadline - now);
    match tokio::time::timeout(budget, page.get_title()).await {
        Ok(Ok(title)) => title,
        _ => None,
    }
}

/// [`probe_page_title`] with a fresh single-probe deadline.
pub(super) async fn probe_page_title_once(page: &chromiumoxide::Page) -> Option<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(PAGE_PROBE_TIMEOUT_MS);
    probe_page_title(page, deadline).await
}

/// [`probe_page_url`] with a fresh single-probe deadline.
pub(super) async fn probe_page_url_once(page: &chromiumoxide::Page) -> Option<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(PAGE_PROBE_TIMEOUT_MS);
    probe_page_url(page, deadline).await
}

/// True for URLs that carry no page content: Chrome's startup tab and the
/// placeholder [`BrowserManager::get_page`] creates when a profile has none.
pub(super) fn is_blank_url(url: &str) -> bool {
    url.is_empty() || url == "about:blank" || url.starts_with("chrome://new-tab")
}

/// Resolve a CDP target ID to its index in `pages`.
///
/// Target IDs are Chrome's own opaque per-tab identifiers, stable for the
/// life of the tab. They replaced positional `tab_N` ids, which indexed a
/// `HashMap` iteration and so named a different page from call to call.
fn find_target_index(pages: &[chromiumoxide::Page], target_id: &str) -> Result<usize> {
    pages
        .iter()
        .position(|p| p.target_id().inner() == target_id)
        .ok_or_else(|| {
            Error::ToolExecution(
                format!(
                    "tab '{target_id}' not found (have {} open). It may have been closed — \
                     call action 'tabs' for the current targetIds.",
                    pages.len()
                )
                .into(),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager() -> BrowserManager {
        BrowserManager::new(BrowserConfig::default())
    }

    #[test]
    fn blank_urls_are_recognized() {
        assert!(is_blank_url(""));
        assert!(is_blank_url("about:blank"));
        assert!(is_blank_url("chrome://new-tab-page"));
        assert!(!is_blank_url("https://www.instagram.com/cutty13/"));
        // Not blank just because the host is unusual.
        assert!(!is_blank_url("about:config"));
    }

    #[test]
    fn protocol_health_summarizes_rejected_methods_without_payloads() {
        let health = ProtocolHealth::default();
        health.record_invalid_method("Page.frameNavigated".to_string(), "first".to_string());
        health.record_invalid_method("Page.frameNavigated".to_string(), "second".to_string());
        health.record_invalid_method("Runtime.consoleAPICalled".to_string(), "latest".to_string());
        assert_eq!(health.invalid_messages.load(Ordering::Relaxed), 3);
        let summary = health.invalid_method_summary();
        assert_eq!(summary[0]["method"], "Page.frameNavigated");
        assert_eq!(summary[0]["count"], 2);
        assert_eq!(summary[1]["method"], "Runtime.consoleAPICalled");
        assert_eq!(health.last_invalid_error().as_deref(), Some("latest"));
    }

    #[test]
    fn only_the_known_chrome_152_policy_rename_is_tolerated() {
        let renamed = serde_json::json!({
            "method": "Network.requestWillBeSentExtraInfo",
            "params": {
                "clientSecurityState": {
                    "initiatorIsSecureContext": true,
                    "initiatorIPAddressSpace": "Public",
                    "localNetworkAccessRequestPolicy": "Allow"
                }
            }
        });
        assert!(known_tolerated_protocol_drift(
            "Network.requestWillBeSentExtraInfo",
            Some(&renamed)
        )
        .is_some());

        let unknown = serde_json::json!({
            "method": "Network.requestWillBeSentExtraInfo",
            "params": { "clientSecurityState": { "futureField": true } }
        });
        assert!(known_tolerated_protocol_drift(
            "Network.requestWillBeSentExtraInfo",
            Some(&unknown)
        )
        .is_none());
        assert!(known_tolerated_protocol_drift("Page.futureEvent", Some(&renamed)).is_none());
    }

    #[test]
    fn only_the_implicit_default_data_root_borrows_the_system_profile() {
        let default = BrowserConfig::default();
        assert!(should_borrow_system_profile(&default, "rustykrab"));

        let mut explicit = BrowserConfig::default();
        explicit
            .profiles
            .get_mut("rustykrab")
            .unwrap()
            .user_data_dir = Some("/tmp/dedicated-browser-profile".into());
        assert!(!should_borrow_system_profile(&explicit, "rustykrab"));

        let isolated = BrowserConfig {
            isolated_root: Some("/tmp/isolated-browser-root".into()),
            ..BrowserConfig::default()
        };
        assert!(!should_borrow_system_profile(&isolated, "rustykrab"));
    }

    #[test]
    fn sticky_target_round_trips() {
        let mgr = manager();
        assert_eq!(mgr.sticky_target("conv-a", "default"), None);

        mgr.set_sticky_target("conv-a", "default", "TARGET-1");
        assert_eq!(
            mgr.sticky_target("conv-a", "default").as_deref(),
            Some("TARGET-1")
        );

        mgr.clear_sticky_target("conv-a", "default");
        assert_eq!(mgr.sticky_target("conv-a", "default"), None);
    }

    #[tokio::test]
    async fn a_pin_with_no_browser_is_definitely_dead() {
        // The relaunch path: Chrome went away, so a pin naming one of its
        // tabs cannot survive. This must be a definite answer, or the pin
        // would be kept forever against a browser that no longer exists.
        let mgr = manager();
        assert_eq!(
            mgr.target_is_live("nonexistent", "SOME-TARGET").await,
            Some(false)
        );
    }

    /// A tab spinning in JavaScript must not stall work on the others.
    ///
    /// `get_title()` is a real round trip to the renderer, so a page in a
    /// `while(true)` loop never answers it; measured unbounded, it had still
    /// not returned after 40s. `tabs` reads a title per tab, so one wedged
    /// page used to hang the whole listing well past any tool-call budget.
    /// (`url()` is served from the handler's cache and returns in
    /// microseconds even then — it is not the exposure.)
    ///
    /// Launches a real headless Chrome; ignored by default.
    #[tokio::test]
    #[ignore = "launches a real Chrome"]
    async fn live_a_wedged_tab_cannot_stall_the_others() {
        const WEDGE: &str = "data:text/html,<title>wedge</title>\
<script>setTimeout(()=>{while(true){}},1500)</script>";
        const NORMAL: &str = "data:text/html,<title>normal</title><h1>ok</h1>";

        let dir = tempfile::tempdir().expect("tempdir");
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("free port");
            l.local_addr().unwrap().port()
        };
        let profile = "wedged-tab-test";
        let mut config = BrowserConfig {
            default_profile: profile.to_string(),
            ..Default::default()
        };
        config.profiles.insert(
            profile.to_string(),
            super::super::config::BrowserProfile {
                cdp_port: Some(port),
                user_data_dir: Some(dir.path().display().to_string()),
                headless: Some(true),
                ..Default::default()
            },
        );
        let mgr = BrowserManager::new(config);
        mgr.start(profile).await.expect("launch chrome");

        mgr.open_tab(profile, NORMAL).await.expect("open normal");
        mgr.open_tab(profile, WEDGE).await.expect("open wedge");
        // Let the wedge tab enter its loop.
        tokio::time::sleep(Duration::from_secs(3)).await;

        // Every probe shares one budget, so the listing is bounded by
        // RESOLVE_PROBE_BUDGET_MS however many tabs are stuck.
        let started = std::time::Instant::now();
        let tabs = mgr.tabs(profile).await.expect("tabs must not hang");
        let elapsed = started.elapsed();
        eprintln!("tabs() with a wedged tab -> {:?}", elapsed);
        assert!(
            elapsed < Duration::from_millis(RESOLVE_PROBE_BUDGET_MS + 2_000),
            "tabs() took {elapsed:?}"
        );

        // The healthy tab is still fully described; only the wedged one
        // loses its title.
        let listed = tabs["tabs"].as_array().expect("tabs array");
        assert!(
            listed.iter().any(|t| t["title"] == "normal"),
            "the healthy tab should still report its title: {tabs}"
        );

        // Resolution stays fast, and must not pick the wedged tab over the
        // healthy one just because it could not be read.
        let started = std::time::Instant::now();
        let page = mgr.get_page(profile, None).await.expect("resolve");
        eprintln!(
            "get_page(None) with a wedged tab -> {:?}",
            started.elapsed()
        );
        assert!(started.elapsed() < Duration::from_secs(6));
        assert!(!page.target_id().inner().is_empty());

        let _ = mgr.stop(profile).await;
    }

    /// Live regression check for the bug this addressing scheme replaced.
    ///
    /// Chrome always has a blank startup tab, and `pages()` walks a
    /// `HashMap`, so the old `pages()[0]` resolution could hand a read the
    /// blank tab moments after a navigate loaded content into another one.
    /// Launches a real headless Chrome; ignored by default:
    ///
    /// ```sh
    /// cargo test -p rustykrab-tools --no-default-features \
    ///   browser::manager::tests::live -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore = "launches a real Chrome"]
    async fn live_resolution_never_lands_on_the_blank_tab() {
        const CONTENT: &str = "data:text/html,<title>cutty13</title><h1>profile</h1>";
        let dir = tempfile::tempdir().expect("tempdir");
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("free port");
            l.local_addr().unwrap().port()
        };
        let profile = "tab-resolution-test";

        let mut config = BrowserConfig {
            default_profile: profile.to_string(),
            ..Default::default()
        };
        config.profiles.insert(
            profile.to_string(),
            super::super::config::BrowserProfile {
                cdp_port: Some(port),
                user_data_dir: Some(dir.path().display().to_string()),
                headless: Some(true),
                ..Default::default()
            },
        );

        let mgr = BrowserManager::new(config);
        mgr.start(profile).await.expect("launch chrome");

        // The hazard has to actually be present, or the test proves nothing:
        // a blank startup tab sitting alongside the page we care about.
        let startup = mgr.get_page(profile, None).await.expect("startup page");
        let startup_url = startup.url().await.ok().flatten().unwrap_or_default();
        assert!(
            is_blank_url(&startup_url),
            "expected a blank startup tab, got {startup_url}"
        );

        let opened = mgr.open_tab(profile, CONTENT).await.expect("open content");
        let content_id = opened["targetId"].as_str().expect("targetId").to_string();
        assert!(
            !content_id.starts_with("tab_"),
            "targetId must be Chrome's own id, got {content_id}"
        );

        // Resolution must be stable: same page every call, never the blank
        // tab. Meanwhile record what the old `pages()[0]` rule would have
        // returned, so the run reports whether the hazard fired.
        let mut raw_blank_hits = 0;
        for i in 0..20 {
            let page = mgr.get_page(profile, None).await.expect("resolve page");
            let url = page.url().await.ok().flatten().unwrap_or_default();
            assert!(!is_blank_url(&url), "iteration {i} resolved to a blank tab");
            assert_eq!(
                page.target_id().inner(),
                &content_id,
                "iteration {i} resolved to a different tab"
            );

            let instances = mgr.instances.lock().await;
            let pages = instances[profile].browser.pages().await.expect("pages");
            let first = pages.first().expect("at least one page");
            let first_url = first.url().await.ok().flatten().unwrap_or_default();
            if is_blank_url(&first_url) {
                raw_blank_hits += 1;
            }
        }
        eprintln!("old pages()[0] rule would have hit the blank tab {raw_blank_hits}/20 times");

        // An id that no longer names a tab is an error, not a silent
        // fallback to some other page.
        let err = mgr
            .get_page(profile, Some("NO-SUCH-TARGET"))
            .await
            .expect_err("stale target must fail");
        assert!(err.to_string().contains("not found"), "{err}");

        // The listing is stable across calls too.
        let first_listing = mgr.tabs(profile).await.expect("tabs");
        let second_listing = mgr.tabs(profile).await.expect("tabs");
        assert_eq!(first_listing["tabs"], second_listing["tabs"]);

        let _ = mgr.stop(profile).await;
    }

    #[test]
    fn sticky_targets_are_scoped_per_session_and_profile() {
        let mgr = manager();
        mgr.set_sticky_target("conv-a", "default", "TARGET-A");
        mgr.set_sticky_target("conv-b", "default", "TARGET-B");
        mgr.set_sticky_target("conv-a", "work", "TARGET-C");

        // Two conversations browsing the same profile must not steer each
        // other's page.
        assert_eq!(
            mgr.sticky_target("conv-a", "default").as_deref(),
            Some("TARGET-A")
        );
        assert_eq!(
            mgr.sticky_target("conv-b", "default").as_deref(),
            Some("TARGET-B")
        );
        assert_eq!(
            mgr.sticky_target("conv-a", "work").as_deref(),
            Some("TARGET-C")
        );

        mgr.clear_sticky_target("conv-a", "default");
        assert_eq!(mgr.sticky_target("conv-a", "default"), None);
        assert_eq!(
            mgr.sticky_target("conv-b", "default").as_deref(),
            Some("TARGET-B")
        );
    }

    /// The reap must not be able to close a tab another session is on.
    /// One Chrome profile is shared by every concurrent run, so this is
    /// the difference between tidying and taking a page out from under
    /// an agent mid-task.
    #[test]
    fn pinned_targets_are_gathered_across_sessions_for_one_profile() {
        let mgr = manager();
        mgr.set_sticky_target("session-a", "rustykrab", "TARGET-A");
        mgr.set_sticky_target("session-b", "rustykrab", "TARGET-B");
        mgr.set_sticky_target("session-c", "other-profile", "TARGET-C");

        let pinned = mgr.pinned_targets("rustykrab");
        assert_eq!(pinned.len(), 2, "{pinned:?}");
        assert!(pinned.contains("TARGET-A"));
        assert!(pinned.contains("TARGET-B"));
        assert!(
            !pinned.contains("TARGET-C"),
            "another profile's pin is not this profile's business: {pinned:?}"
        );
    }

    #[test]
    fn a_profile_with_no_pins_has_nothing_to_protect() {
        assert!(manager().pinned_targets("rustykrab").is_empty());
    }
}
