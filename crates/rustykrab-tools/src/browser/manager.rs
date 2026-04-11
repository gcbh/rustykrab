//! Multi-profile browser manager modeled after OpenClaw's architecture.
//!
//! Manages multiple isolated browser instances, each with its own CDP port,
//! user-data directory, and lifecycle. The manager handles:
//! - Launching/connecting to Chrome instances per profile
//! - Tab management (list, open, close, focus) with targetId addressing
//! - Browser lifecycle (start, stop, status)
//! - Chrome profile symlink setup for cookie/session persistence

use chromiumoxide::Browser;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_stream::StreamExt;

use super::config::{BrowserConfig, DriverType};
use rustykrab_core::{Error, Result};

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
}

impl BrowserManager {
    pub fn new(config: BrowserConfig) -> Self {
        Self {
            config,
            instances: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Load configuration and create a manager.
    pub fn from_config() -> Self {
        Self::new(BrowserConfig::load())
    }

    pub fn config(&self) -> &BrowserConfig {
        &self.config
    }

    /// Get or create a browser instance for the given profile.
    pub async fn get_browser(
        &self,
        profile_name: &str,
    ) -> Result<Arc<Mutex<HashMap<String, ProfileInstance>>>> {
        let mut instances = self.instances.lock().await;

        if !instances.contains_key(profile_name) {
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
            let page_count = inst.browser.pages().await.map(|p| p.len()).unwrap_or(0);
            serde_json::json!({
                "status": "running",
                "profile": profile_name,
                "cdp_url": inst.cdp_url,
                "launched_by_us": inst.launched_by_us,
                "tabs": page_count
            })
        } else {
            // Try to probe the CDP endpoint
            let cdp_url = self.config.resolve_cdp_url(profile_name);
            let reachable = probe_cdp(&cdp_url).await;
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
        if instances.contains_key(profile_name) {
            return Ok(serde_json::json!({
                "status": "already_running",
                "profile": profile_name
            }));
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
            // Abort the handler task to clean up the CDP connection
            inst._handler_task.abort();
            // Drop the browser — this closes the CDP connection
            drop(inst.browser);
            Ok(serde_json::json!({
                "status": "stopped",
                "profile": profile_name
            }))
        } else {
            Ok(serde_json::json!({
                "status": "not_running",
                "profile": profile_name
            }))
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

        let page = inst
            .browser
            .new_page(url)
            .await
            .map_err(|e| Error::ToolExecution(format!("failed to open tab: {e}").into()))?;

        let _ = page.wait_for_navigation().await;
        let actual_url = page.url().await.ok().flatten().unwrap_or_default();
        let title = page.get_title().await.ok().flatten().unwrap_or_default();
        let pages = inst.browser.pages().await.map(|p| p.len()).unwrap_or(0);

        Ok(serde_json::json!({
            "status": "opened",
            "url": actual_url,
            "title": title,
            "targetId": format!("tab_{}", pages - 1),
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

        // Navigate to about:blank then close — close via dropping the page
        let page = &pages[idx];
        let _ = page.goto("about:blank").await;
        // chromiumoxide doesn't have a direct close_page; we close by executing
        // the CDP command
        let _ = page.evaluate("window.close()").await;

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

        // Try connecting to an existing instance first
        match Browser::connect(&cdp_url).await {
            Ok((browser, handler)) => {
                let mut handler = handler;
                let handler_task =
                    tokio::spawn(async move { while let Some(_event) = handler.next().await {} });
                return Ok(ProfileInstance {
                    browser,
                    _handler_task: handler_task,
                    profile_name: profile_name.to_string(),
                    cdp_url,
                    launched_by_us: false,
                });
            }
            Err(e) => {
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
        }

        // Launch a new browser instance via spawn_blocking to avoid
        // blocking the async runtime with std::fs and std::process
        // operations (fixes ASYNC-H3).
        let config = self.config.clone();
        let profile = profile_name.to_string();
        tokio::task::spawn_blocking(move || launch_browser_blocking(&config, &profile))
            .await
            .map_err(|e| Error::ToolExecution(format!("launch task failed: {e}").into()))??;

        // Wait for it to start
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let (browser, handler) = Browser::connect(&cdp_url).await.map_err(|e| {
            Error::ToolExecution(
                format!(
                    "browser not reachable at {cdp_url} after launch attempt: {e}. \
                     If a browser is already running without remote debugging, \
                     quit it first so a new instance can start."
                )
                .into(),
            )
        })?;

        let mut handler = handler;
        let handler_task =
            tokio::spawn(async move { while let Some(_event) = handler.next().await {} });

        Ok(ProfileInstance {
            browser,
            _handler_task: handler_task,
            profile_name: profile_name.to_string(),
            cdp_url,
            launched_by_us: true,
        })
    }
}

/// Launch a Chrome/Chromium instance for the given profile.
///
/// This is a free function (not a method) so it can be called from
/// `spawn_blocking` without lifetime issues (fixes ASYNC-H3).
fn launch_browser_blocking(config: &BrowserConfig, profile_name: &str) -> Result<()> {
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

    std::process::Command::new(&exe)
        .args(&args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| {
            Error::ToolExecution(format!("failed to launch browser ({exe}): {e}").into())
        })?;

    tracing::info!(
        port,
        profile = profile_name,
        %profile_dir_name,
        "launched browser with remote debugging"
    );
    Ok(())
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

/// Probe a CDP URL to check if a browser is reachable.
async fn probe_cdp(url: &str) -> bool {
    let version_url = format!("{}/json/version", url.trim_end_matches('/'));
    reqwest::get(&version_url)
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
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
