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

use chromiumoxide::Browser;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_stream::StreamExt;

use super::config::{BrowserConfig, DriverType};
use rustykrab_core::{Error, Result};

/// How long to wait for a launched Chrome to start serving CDP before giving
/// up. Generous so cold launches on slow disks still succeed.
const POST_LAUNCH_TIMEOUT_MS: u64 = 15_000;

/// Cap on the per-probe HTTP timeout when polling `/json/version`.
const HEALTH_PROBE_TIMEOUT_MS: u64 = 3_000;

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
                dead._handler_task.abort();
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
            serde_json::json!({
                "status": if alive { "running" } else { "unresponsive" },
                "profile": profile_name,
                "cdp_url": inst.cdp_url,
                "launched_by_us": inst.launched_by_us,
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
                dead._handler_task.abort();
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
            inst._handler_task.abort();
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
        let mut rows = Vec::new();
        for page in pages.iter() {
            let url = page.url().await.ok().flatten().unwrap_or_default();
            let title = page.get_title().await.ok().flatten().unwrap_or_default();
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
                for page in existing {
                    if pinned.contains(page.target_id().inner()) {
                        continue;
                    }
                    let url = page.url().await.ok().flatten().unwrap_or_default();
                    if is_blank_url(&url) {
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
        let page = tokio::time::timeout(nav_timeout, inst.browser.new_page(url))
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

        // Bound wait_for_navigation so a slow page can't hang the call forever.
        let nav_started = std::time::Instant::now();
        let settled = tokio::time::timeout(nav_timeout, page.wait_for_navigation())
            .await
            .is_ok();
        tracing::info!(
            url,
            open_ms,
            settle_ms = nav_started.elapsed().as_millis() as u64,
            settled,
            "opened tab"
        );
        let actual_url = page.url().await.ok().flatten().unwrap_or_default();
        let title = page.get_title().await.ok().flatten().unwrap_or_default();

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

        let url = page.url().await.ok().flatten().unwrap_or_default();
        let title = page.get_title().await.ok().flatten().unwrap_or_default();

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

    /// Whether `target_id` still names a live tab in this profile.
    pub async fn target_is_live(&self, profile_name: &str, target_id: &str) -> bool {
        let instances = self.instances.lock().await;
        let Some(inst) = instances.get(profile_name) else {
            return false;
        };
        let Ok(pages) = inst.browser.pages().await else {
            return false;
        };
        pages.iter().any(|p| p.target_id().inner() == target_id)
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
        let mut ranked: Vec<(bool, String, usize)> = Vec::with_capacity(pages.len());
        for (i, page) in pages.iter().enumerate() {
            let url = page.url().await.ok().flatten().unwrap_or_default();
            ranked.push((is_blank_url(&url), page.target_id().inner().clone(), i));
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
        match tokio::time::timeout(connect_timeout, Browser::connect(&cdp_url)).await {
            Ok(Ok((browser, handler))) => {
                let handler_task = spawn_handler_task(handler, profile_name.to_string());
                return Ok(ProfileInstance {
                    browser,
                    _handler_task: handler_task,
                    profile_name: profile_name.to_string(),
                    cdp_url,
                    launched_by_us: false,
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
            match tokio::time::timeout(connect_timeout, Browser::connect(&cdp_url)).await {
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

        let handler_task = spawn_handler_task(handler, profile_name.to_string());

        Ok(ProfileInstance {
            browser,
            _handler_task: handler_task,
            profile_name: profile_name.to_string(),
            cdp_url,
            launched_by_us: true,
        })
    }

    fn health_probe_timeout_ms(&self) -> u64 {
        self.config
            .remote_cdp_timeout_ms
            .min(HEALTH_PROBE_TIMEOUT_MS)
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
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(event) = handler.next().await {
            if let Err(e) = event {
                tracing::debug!(profile = %profile_name, error = %e, "CDP handler event error");
            }
        }
        tracing::info!(profile = %profile_name, "browser CDP handler task exited");
    })
}

/// Health check for a cached `ProfileInstance`. We do a cheap CDP
/// round-trip (`Browser.getVersion`) with a short timeout. A failure
/// (timeout or CDP error) means the browser has gone away.
async fn is_instance_alive(inst: &ProfileInstance) -> bool {
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
        setup_profile_link(&user_data_dir)
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
fn setup_profile_link(user_data_dir: &std::path::Path) -> String {
    let Some(chrome_dir) = chrome_data_dir() else {
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

    // Write minimal Local State to disable profile picker
    let local_state_dest = user_data_dir.join("Local State");
    let local_state = serde_json::json!({
        "profile": {
            "last_used": &profile_name,
            "last_active_profiles": [&profile_name],
            "picker_shown": false
        }
    });
    if let Err(e) = std::fs::write(&local_state_dest, local_state.to_string()) {
        tracing::warn!("could not write Chrome Local State: {e}");
    }

    profile_name
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
