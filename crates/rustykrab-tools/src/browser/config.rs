//! Browser configuration types modeled after OpenClaw's browser management.
//!
//! Supports multi-profile browser management with per-profile CDP ports,
//! user-data directories, and driver types.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Top-level browser configuration, loaded from `~/.rustykrab/browser.json`
/// or environment variables.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserConfig {
    /// Master switch for the browser subsystem.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Whether `evaluate` (arbitrary JS) is allowed.
    /// Defaults to false for security — arbitrary JS can access cookies,
    /// session tokens, and other sensitive data in the browser context.
    #[serde(default)]
    pub evaluate_enabled: bool,

    /// Default profile name used when `profile` is omitted from a tool call.
    #[serde(default = "default_profile_name")]
    pub default_profile: String,

    /// Run browsers in headless mode.
    #[serde(default)]
    pub headless: bool,

    /// Disable the Chromium sandbox (needed on some Linux setups).
    #[serde(default)]
    pub no_sandbox: bool,

    /// Only attach to an existing browser; never launch one.
    #[serde(default)]
    pub attach_only: bool,

    /// Override browser executable path.
    #[serde(default)]
    pub executable_path: Option<String>,

    /// Starting port for the CDP port range (profiles get sequential ports).
    #[serde(default = "default_cdp_port_start")]
    pub cdp_port_range_start: u16,

    /// Timeout for remote CDP connections (ms).
    #[serde(default = "default_remote_cdp_timeout")]
    pub remote_cdp_timeout_ms: u64,

    /// Named browser profiles.
    #[serde(default)]
    pub profiles: HashMap<String, BrowserProfile>,

    /// SSRF protection policy for browser navigation.
    #[serde(default)]
    pub ssrf_policy: SsrfPolicy,

    /// Extra arguments to pass to the browser on launch.
    #[serde(default)]
    pub extra_args: Vec<String>,
}

/// A named browser profile — an isolated Chrome instance with its own
/// user-data directory and CDP port.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserProfile {
    /// CDP port for this profile (local managed instances).
    #[serde(default)]
    pub cdp_port: Option<u16>,

    /// Remote CDP URL (for connecting to a browser running elsewhere).
    #[serde(default)]
    pub cdp_url: Option<String>,

    /// Custom user-data directory override.
    #[serde(default)]
    pub user_data_dir: Option<String>,

    /// Driver type for this profile.
    #[serde(default)]
    pub driver: DriverType,

    /// Only attach; never launch for this profile.
    #[serde(default)]
    pub attach_only: Option<bool>,

    /// Headless mode override for this profile.
    #[serde(default)]
    pub headless: Option<bool>,

    /// No-sandbox override for this profile.
    #[serde(default)]
    pub no_sandbox: Option<bool>,

    /// Override executable path for this profile.
    #[serde(default)]
    pub executable_path: Option<String>,

    /// Display color tag (for UI identification).
    #[serde(default = "default_color")]
    pub color: String,
}

/// How a browser instance is driven.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum DriverType {
    /// Managed: RustyKrab launches and owns the browser process.
    #[default]
    Rustykrab,
    /// Existing session: attach to user's running Chrome via CDP.
    ExistingSession,
    /// Remote: connect to a remote CDP endpoint.
    Remote,
}

/// SSRF policy for browser navigation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SsrfPolicy {
    /// Allow navigation to private network addresses.
    #[serde(default)]
    pub allow_private_network: bool,

    /// Hostnames explicitly allowed regardless of SSRF rules.
    #[serde(default)]
    pub hostname_allowlist: Vec<String>,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        let mut profiles = HashMap::new();
        profiles.insert(
            "rustykrab".to_string(),
            BrowserProfile {
                cdp_port: Some(18800),
                cdp_url: None,
                user_data_dir: None,
                driver: DriverType::Rustykrab,
                attach_only: None,
                headless: None,
                no_sandbox: None,
                executable_path: None,
                color: "#FF6B00".to_string(),
            },
        );
        Self {
            enabled: true,
            evaluate_enabled: false,
            default_profile: "rustykrab".to_string(),
            headless: false,
            no_sandbox: false,
            attach_only: false,
            executable_path: None,
            cdp_port_range_start: 18800,
            remote_cdp_timeout_ms: 5000,
            profiles,
            ssrf_policy: SsrfPolicy::default(),
            extra_args: Vec::new(),
        }
    }
}

impl Default for BrowserProfile {
    fn default() -> Self {
        Self {
            cdp_port: None,
            cdp_url: None,
            user_data_dir: None,
            driver: DriverType::Rustykrab,
            attach_only: None,
            headless: None,
            no_sandbox: None,
            executable_path: None,
            color: default_color(),
        }
    }
}

impl BrowserConfig {
    /// Load config from `~/.rustykrab/browser.json`, falling back to defaults.
    /// Environment variables override file settings.
    pub fn load() -> Self {
        let mut config = Self::load_from_file().unwrap_or_default();

        // Env overrides
        if let Ok(url) = std::env::var("CHROME_CDP_URL") {
            // Legacy single-URL mode: set as default profile's cdp_url
            let profile = config
                .profiles
                .entry(config.default_profile.clone())
                .or_default();
            profile.cdp_url = Some(url);
            profile.driver = DriverType::ExistingSession;
        }
        if let Ok(port) = std::env::var("CHROME_CDP_PORT") {
            if let Ok(p) = port.parse::<u16>() {
                let profile = config
                    .profiles
                    .entry(config.default_profile.clone())
                    .or_default();
                profile.cdp_port = Some(p);
            }
        }
        if let Ok(path) = std::env::var("CHROME_EXECUTABLE") {
            config.executable_path = Some(path);
        }
        if std::env::var("BROWSER_HEADLESS").as_deref() == Ok("1") {
            config.headless = true;
        }
        if std::env::var("BROWSER_NO_SANDBOX").as_deref() == Ok("1") {
            config.no_sandbox = true;
        }

        config
    }

    fn load_from_file() -> Option<Self> {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .ok()?;
        let path = PathBuf::from(home).join(".rustykrab").join("browser.json");
        let data = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&data).ok()
    }

    /// Resolve the effective CDP URL for a profile.
    pub fn resolve_cdp_url(&self, profile_name: &str) -> String {
        if let Some(profile) = self.profiles.get(profile_name) {
            if let Some(ref url) = profile.cdp_url {
                return url.clone();
            }
            if let Some(port) = profile.cdp_port {
                return format!("http://127.0.0.1:{port}");
            }
        }
        // Fallback: derive port from range
        let idx = self
            .profiles
            .keys()
            .position(|k| k == profile_name)
            .unwrap_or(0) as u16;
        format!("http://127.0.0.1:{}", self.cdp_port_range_start + idx)
    }

    /// Resolve the user-data directory for a profile.
    pub fn resolve_user_data_dir(&self, profile_name: &str) -> PathBuf {
        if let Some(profile) = self.profiles.get(profile_name) {
            if let Some(ref dir) = profile.user_data_dir {
                return PathBuf::from(dir);
            }
        }
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home)
            .join(".rustykrab")
            .join("browser")
            .join(profile_name)
            .join("user-data")
    }

    /// Whether a profile should use headless mode.
    pub fn is_headless(&self, profile_name: &str) -> bool {
        self.profiles
            .get(profile_name)
            .and_then(|p| p.headless)
            .unwrap_or(self.headless)
    }

    /// Whether a profile should disable the sandbox.
    pub fn is_no_sandbox(&self, profile_name: &str) -> bool {
        self.profiles
            .get(profile_name)
            .and_then(|p| p.no_sandbox)
            .unwrap_or(self.no_sandbox)
    }

    /// Whether a profile is attach-only.
    pub fn is_attach_only(&self, profile_name: &str) -> bool {
        self.profiles
            .get(profile_name)
            .and_then(|p| p.attach_only)
            .unwrap_or(self.attach_only)
    }

    /// Resolve the executable path for a profile.
    pub fn resolve_executable(&self, profile_name: &str) -> Option<String> {
        self.profiles
            .get(profile_name)
            .and_then(|p| p.executable_path.clone())
            .or_else(|| self.executable_path.clone())
    }
}

fn default_true() -> bool {
    true
}

fn default_profile_name() -> String {
    "rustykrab".to_string()
}

fn default_cdp_port_start() -> u16 {
    18800
}

fn default_remote_cdp_timeout() -> u64 {
    5000
}

fn default_color() -> String {
    "#FF6B00".to_string()
}
