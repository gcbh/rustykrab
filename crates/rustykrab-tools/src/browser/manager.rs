//! Multi-profile browser manager modeled after OpenClaw's architecture.
//!
//! Manages multiple isolated browser instances, each with its own CDP port,
//! user-data directory, and lifecycle. The manager handles:
//! - Launching/connecting to Chrome instances per profile
//! - Tab management (list, open, close, focus) with targetId addressing
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
}

impl BrowserManager {
    pub fn new(config: BrowserConfig) -> Self {
        let mgr = Self {
            config,
            instances: Arc::new(Mutex::new(HashMap::new())),
            children: Arc::new(std::sync::Mutex::new(HashMap::new())),
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

        let mut tabs = Vec::new();
        for (i, page) in pages.iter().enumerate() {
            let url = page.url().await.ok().flatten().unwrap_or_default();
            let title = page.get_title().await.ok().flatten().unwrap_or_default();
            // Use the page's target ID for deterministic addressing
            let target_id = format!("tab_{i}");
            tabs.push(serde_json::json!({
                "targetId": target_id,
                "index": i,
                "url": url,
                "title": title,
            }));
        }

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

        let page = tokio::time::timeout(nav_timeout, inst.browser.new_page(url))
            .await
            .map_err(|_| {
                Error::ToolExecution(
                    format!("open_tab timed out after {}ms", nav_timeout.as_millis()).into(),
                )
            })?
            .map_err(|e| Error::ToolExecution(format!("failed to open tab: {e}").into()))?;

        // Bound wait_for_navigation so a slow page can't hang the call forever.
        let _ = tokio::time::timeout(nav_timeout, page.wait_for_navigation()).await;
        let actual_url = page.url().await.ok().flatten().unwrap_or_default();
        let title = page.get_title().await.ok().flatten().unwrap_or_default();
        let pages = inst.browser.pages().await.map(|p| p.len()).unwrap_or(0);

        Ok(serde_json::json!({
            "status": "opened",
            "url": actual_url,
            "title": title,
            "targetId": format!("tab_{}", pages.saturating_sub(1)),
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

        let idx = parse_tab_index(target_id)?;
        let mut pages = inst
            .browser
            .pages()
            .await
            .map_err(|e| Error::ToolExecution(format!("failed to list tabs: {e}").into()))?;

        if idx >= pages.len() {
            return Err(Error::ToolExecution(
                format!("tab index {idx} out of range (have {} tabs)", pages.len()).into(),
            ));
        }

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

        let idx = parse_tab_index(target_id)?;
        let pages = inst
            .browser
            .pages()
            .await
            .map_err(|e| Error::ToolExecution(format!("failed to list tabs: {e}").into()))?;

        if idx >= pages.len() {
            return Err(Error::ToolExecution(
                format!("tab index {idx} out of range (have {} tabs)", pages.len()).into(),
            ));
        }

        let page = &pages[idx];
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

    /// Get a specific page by targetId, or the first active page.
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
            let idx = parse_tab_index(tid)?;
            if idx < pages.len() {
                Ok(pages.into_iter().nth(idx).unwrap())
            } else {
                Err(Error::ToolExecution(
                    format!("tab {tid} not found (have {} tabs)", pages.len()).into(),
                ))
            }
        } else if let Some(page) = pages.into_iter().next() {
            Ok(page)
        } else {
            // No pages — create one
            inst.browser
                .new_page("about:blank")
                .await
                .map_err(|e| Error::ToolExecution(format!("failed to create tab: {e}").into()))
        }
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

    let mut cmd = std::process::Command::new(&exe);
    cmd.args(&args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

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

/// Parse a targetId like "tab_3" into an index.
fn parse_tab_index(target_id: &str) -> Result<usize> {
    let idx_str = target_id.strip_prefix("tab_").unwrap_or(target_id);
    idx_str.parse::<usize>().map_err(|_| {
        Error::ToolExecution(
            format!("invalid targetId '{target_id}'. Expected format: 'tab_N' (e.g., 'tab_0')")
                .into(),
        )
    })
}
