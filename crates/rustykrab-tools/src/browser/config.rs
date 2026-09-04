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

    /// Root for browser profile data, when the caller wants a browser of
    /// its own rather than the account's.
    ///
    /// Set it and two things follow: profile data lives under this root
    /// instead of `~/.rustykrab/browser`, and the symlink to the real
    /// Chrome profile is suppressed. Unset -- the normal case -- nothing
    /// changes, and the real profile is borrowed because that is where
    /// the logins are.
    ///
    /// The obvious way to get an isolated browser is to point HOME at a
    /// scratch directory. That does not work: Chrome wedges its renderer
    /// when HOME is an empty directory, committing a page's URL while the
    /// document never arrives. Isolate this one directory instead.
    #[serde(default)]
    pub isolated_root: Option<PathBuf>,

    /// Optional root for browser downloads. Each profile receives its own
    /// subdirectory. When omitted, downloads live under that profile's
    /// RustyKrab user-data directory.
    #[serde(default)]
    pub download_root: Option<PathBuf>,

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

    /// Maximum time for one Chrome DevTools Protocol request (ms), clamped to
    /// 500..=10,000 by the driver.
    ///
    /// This is deliberately below the browser tool and action deadlines. A
    /// single missing response must surface with its CDP method/stage while
    /// there is still time to reconnect the browser session.
    #[serde(default = "default_cdp_request_timeout")]
    pub cdp_request_timeout_ms: u64,

    /// Named browser profiles.
    #[serde(default)]
    pub profiles: HashMap<String, BrowserProfile>,

    /// SSRF protection policy for browser navigation.
    #[serde(default)]
    pub ssrf_policy: SsrfPolicy,

    /// How JavaScript dialogs opened by browser actions are handled.
    #[serde(default)]
    pub dialog_policy: DialogPolicy,

    /// Automatically focus the only new, policy-approved tab opened by an
    /// action. Multiple simultaneous popups are reported but not guessed at.
    #[serde(default = "default_true")]
    pub auto_focus_new_tabs: bool,

    /// Allow RustyKrab to change browser-wide download behavior when attached
    /// to a Chrome process it did not launch. Off by default because Chrome's
    /// Browser.setDownloadBehavior affects the operator's own tabs too.
    #[serde(default)]
    pub allow_attached_downloads: bool,

    /// Disable Chrome site isolation as an opt-in compatibility escape hatch.
    /// The default remains secure: the OOPIF bridge handles cross-origin frames
    /// without weakening Chromium's process boundary.
    #[serde(default)]
    pub disable_site_isolation: bool,

    /// Permit the active model to mark visible browser interactions as
    /// CAPTCHA-solving attempts. The model receives no bypass API: it uses
    /// the same screenshot, ref, coordinate, and keyboard actions as any
    /// other page interaction, under the budgets below.
    #[serde(default)]
    pub model_captcha_solver: bool,

    /// Maximum explicitly tagged model interactions in one challenge episode.
    #[serde(default = "default_captcha_max_attempts")]
    pub captcha_max_attempts: u32,

    /// Wall-clock budget for one challenge episode.
    #[serde(default = "default_captcha_timeout_ms")]
    pub captcha_timeout_ms: u64,

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

    /// If non-empty, navigation is limited to these exact hosts or explicit
    /// `*.example.com` suffix patterns. Deny rules are still applied first.
    #[serde(default)]
    pub allowed_domains: Vec<String>,

    /// Exact hosts or explicit `*.example.com` suffix patterns that navigation
    /// must never reach, including through redirects and page-opened tabs.
    #[serde(default)]
    pub prohibited_domains: Vec<String>,
}

/// JavaScript dialog handling policy.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum DialogPolicy {
    /// Match browser-use: accept alert/confirm/beforeunload, dismiss prompt.
    #[default]
    Auto,
    /// Accept every dialog, including prompts with an empty response.
    Accept,
    /// Dismiss every dialog.
    Dismiss,
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
            // Unset by default: normal use borrows the real Chrome
            // profile, which is where the logins are.
            isolated_root: None,
            download_root: None,
            headless: false,
            no_sandbox: false,
            attach_only: false,
            executable_path: None,
            cdp_port_range_start: 18800,
            remote_cdp_timeout_ms: 5000,
            cdp_request_timeout_ms: 10_000,
            profiles,
            ssrf_policy: SsrfPolicy::default(),
            dialog_policy: DialogPolicy::default(),
            auto_focus_new_tabs: true,
            allow_attached_downloads: false,
            disable_site_isolation: false,
            model_captcha_solver: false,
            captcha_max_attempts: default_captcha_max_attempts(),
            captcha_timeout_ms: default_captcha_timeout_ms(),
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
        if let Ok(root) = std::env::var("RUSTYKRAB_BROWSER_ISOLATED_ROOT") {
            if !root.is_empty() {
                config.isolated_root = Some(PathBuf::from(root));
            }
        }
        if let Ok(root) = std::env::var("RUSTYKRAB_BROWSER_DOWNLOAD_ROOT") {
            if !root.is_empty() {
                config.download_root = Some(PathBuf::from(root));
            }
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
        if let Some(root) = &self.isolated_root {
            return root.join(profile_name).join("user-data");
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

    /// Resolve the only directory from which completed downloads may be
    /// reported back to the agent.
    pub fn resolve_download_dir(&self, profile_name: &str) -> PathBuf {
        self.download_root
            .as_ref()
            .map(|root| root.join(profile_name))
            .unwrap_or_else(|| self.resolve_user_data_dir(profile_name).join("downloads"))
    }

    /// Whether the browser filesystem is on another host. CDP reports a
    /// download path in that browser's filesystem; without an artifact
    /// transfer channel RustyKrab cannot validate or expose it as a local
    /// path.
    pub fn is_remote_profile(&self, profile_name: &str) -> bool {
        matches!(
            self.profiles
                .get(profile_name)
                .map(|profile| &profile.driver),
            Some(DriverType::Remote)
        )
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

    /// Whether the profile is RustyKrab-owned rather than an operator or
    /// remote browser. Browser-wide settings are safe only for owned profiles
    /// unless the operator opts in explicitly.
    pub fn is_managed_profile(&self, profile_name: &str) -> bool {
        matches!(
            self.profiles
                .get(profile_name)
                .map(|profile| &profile.driver),
            Some(DriverType::Rustykrab) | None
        )
    }

    pub fn effective_captcha_max_attempts(&self) -> u32 {
        self.captcha_max_attempts.clamp(1, 50)
    }

    pub fn effective_captcha_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.captcha_timeout_ms.clamp(5_000, 300_000))
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

fn default_cdp_request_timeout() -> u64 {
    10_000
}

fn default_captcha_max_attempts() -> u32 {
    12
}

fn default_captcha_timeout_ms() -> u64 {
    120_000
}

fn default_color() -> String {
    "#FF6B00".to_string()
}

#[cfg(test)]
mod isolated_root_tests {
    use super::*;

    /// The point of holding this as config rather than reading the
    /// environment where it is used: it can be exercised without mutating
    /// process-global state, which every other test in this binary shares.
    #[test]
    fn an_isolated_root_places_profile_data_under_it() {
        let config = BrowserConfig {
            isolated_root: Some(PathBuf::from("/tmp/trial-7/browser")),
            ..Default::default()
        };
        assert_eq!(
            config.resolve_user_data_dir("rustykrab"),
            PathBuf::from("/tmp/trial-7/browser/rustykrab/user-data")
        );
    }

    #[test]
    fn without_one_the_account_directory_is_used() {
        let dir = BrowserConfig::default().resolve_user_data_dir("rustykrab");
        let shown = dir.display().to_string();
        assert!(shown.contains(".rustykrab/browser/rustykrab"), "{shown}");
    }

    /// Normal use must keep borrowing the real Chrome profile; that is
    /// where the logins are.
    #[test]
    fn isolation_is_off_by_default() {
        assert!(BrowserConfig::default().isolated_root.is_none());
    }

    /// A per-profile override still wins, as it did before.
    #[test]
    fn an_explicit_profile_directory_outranks_the_isolated_root() {
        let mut config = BrowserConfig {
            isolated_root: Some(PathBuf::from("/tmp/trial-7/browser")),
            ..Default::default()
        };
        config
            .profiles
            .entry("rustykrab".to_string())
            .or_default()
            .user_data_dir = Some("/explicit/path".to_string());
        assert_eq!(
            config.resolve_user_data_dir("rustykrab"),
            PathBuf::from("/explicit/path")
        );
    }

    #[test]
    fn cdp_request_timeout_has_a_backward_compatible_json_default() {
        let config: BrowserConfig = serde_json::from_str("{}").expect("browser config");
        assert_eq!(config.cdp_request_timeout_ms, 10_000);

        let configured: BrowserConfig =
            serde_json::from_str(r#"{"cdpRequestTimeoutMs":2500}"#).expect("browser config");
        assert_eq!(configured.cdp_request_timeout_ms, 2_500);
    }

    #[test]
    fn downloads_are_profile_scoped_and_configurable() {
        let default = BrowserConfig {
            isolated_root: Some(PathBuf::from("/tmp/browser-isolation")),
            ..Default::default()
        };
        assert_eq!(
            default.resolve_download_dir("agent"),
            PathBuf::from("/tmp/browser-isolation/agent/user-data/downloads")
        );

        let configured = BrowserConfig {
            download_root: Some(PathBuf::from("/tmp/agent-downloads")),
            ..Default::default()
        };
        assert_eq!(
            configured.resolve_download_dir("agent"),
            PathBuf::from("/tmp/agent-downloads/agent")
        );
    }

    #[test]
    fn remote_profile_is_distinguished_from_local_attach() {
        let mut config = BrowserConfig::default();
        config.profiles.insert(
            "remote".to_string(),
            BrowserProfile {
                driver: DriverType::Remote,
                ..Default::default()
            },
        );
        config.profiles.insert(
            "attached".to_string(),
            BrowserProfile {
                driver: DriverType::ExistingSession,
                ..Default::default()
            },
        );

        assert!(config.is_remote_profile("remote"));
        assert!(!config.is_remote_profile("attached"));
        assert!(!config.is_remote_profile("missing"));
        assert!(config.is_managed_profile("missing"));
        assert!(!config.is_managed_profile("attached"));
    }

    #[test]
    fn browser_use_compatibility_policies_have_explicit_defaults() {
        let config: BrowserConfig = serde_json::from_str("{}").expect("browser config");
        assert_eq!(config.dialog_policy, DialogPolicy::Auto);
        assert!(config.auto_focus_new_tabs);
        assert!(!config.allow_attached_downloads);
        assert!(!config.disable_site_isolation);
        assert!(!config.model_captcha_solver);
        assert_eq!(config.effective_captcha_max_attempts(), 12);
        assert_eq!(
            config.effective_captcha_timeout(),
            std::time::Duration::from_secs(120)
        );
    }

    #[test]
    fn captcha_experiment_config_is_camel_case_and_clamped() {
        let low: BrowserConfig = serde_json::from_str(
            r#"{"modelCaptchaSolver":true,"captchaMaxAttempts":0,"captchaTimeoutMs":1}"#,
        )
        .expect("browser config");
        assert!(low.model_captcha_solver);
        assert_eq!(low.effective_captcha_max_attempts(), 1);
        assert_eq!(
            low.effective_captcha_timeout(),
            std::time::Duration::from_secs(5)
        );

        let high: BrowserConfig =
            serde_json::from_str(r#"{"captchaMaxAttempts":500,"captchaTimeoutMs":9999999}"#)
                .expect("browser config");
        assert_eq!(high.effective_captcha_max_attempts(), 50);
        assert_eq!(
            high.effective_captcha_timeout(),
            std::time::Duration::from_secs(300)
        );
    }
}
