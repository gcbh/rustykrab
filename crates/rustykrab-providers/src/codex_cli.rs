//! ChatGPT-authenticated Codex CLI profiles and sanitized quota observations.
//! Credentials stay with Codex; this adapter only reads its app-server protocol.
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use rustykrab_core::Error;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{ChildStdin, ChildStdout, Command};

const AUTH_TTL: Duration = Duration::from_secs(30);
const BACKOFF_SECONDS: i64 = 300;
const BASE_ENV: [&str; 9] = [
    "PATH", "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "TERM", "TMPDIR", "SHELL",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CodexQuotaWindow {
    pub used_percent: f64,
    pub window_minutes: Option<u64>,
    pub resets_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CodexQuotaBucket {
    pub primary: Option<CodexQuotaWindow>,
    pub secondary: Option<CodexQuotaWindow>,
}

/// No email, account UUID, credential, credits or arbitrary server payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CodexChatGptStatus {
    pub subscription: Option<String>,
    pub account_fingerprint: Option<String>,
    pub codex_home: Option<PathBuf>,
    pub rate_limits: BTreeMap<String, CodexQuotaBucket>,
    pub quota_checked_at: Option<DateTime<Utc>>,
    pub quota_error: Option<String>,
    pub rate_limited_until: Option<DateTime<Utc>>,
    pub error: Option<String>,
}

#[derive(Default)]
struct Cache {
    checked: Option<Instant>,
    status: CodexChatGptStatus,
}

pub struct CodexChatGptRuntime {
    pub command: PathBuf,
    pub codex_home: Option<PathBuf>,
    cache: Mutex<Cache>,
}

impl CodexChatGptRuntime {
    pub fn new(command: PathBuf, codex_home: Option<PathBuf>) -> Self {
        Self {
            command,
            codex_home,
            cache: Mutex::new(Cache::default()),
        }
    }

    /// Select the CLI login, excluding inherited API/provider/remote overrides.
    pub fn isolate(&self, cmd: &mut Command) {
        cmd.env_clear();
        for name in BASE_ENV {
            if let Some(value) = std::env::var_os(name) {
                cmd.env(name, value);
            }
        }
        if let Some(dir) = &self.codex_home {
            cmd.env("CODEX_HOME", dir);
        }
    }

    pub fn status(&self) -> CodexChatGptStatus {
        let mut status = self
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .status
            .clone();
        status.codex_home = self.codex_home.clone();
        if status
            .rate_limited_until
            .is_some_and(|until| until <= Utc::now())
        {
            status.rate_limited_until = None;
        }
        status
    }

    pub fn health_error(&self) -> Option<String> {
        let status = self.status();
        if status.rate_limited_until.is_some() {
            return Some("Codex subscription usage limit observed; waiting for retry".into());
        }
        status.error.or_else(|| {
            status
                .subscription
                .is_none()
                .then(|| "Codex ChatGPT login has not been verified".into())
        })
    }

    pub fn needs_refresh(&self) -> bool {
        !self
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .checked
            .is_some_and(|at| at.elapsed() < AUTH_TTL)
    }

    fn readiness(&self, checked: bool) -> Result<bool, Error> {
        if self.status().rate_limited_until.is_some() {
            return Err(Error::ModelRateLimit(
                "Codex subscription usage limit observed; waiting for retry".into(),
            ));
        }
        self.health_error()
            .map_or(Ok(checked), |why| Err(Error::ModelAuthError(why)))
    }

    /// No inference, login/logout, reset consumption or direct credential reads.
    /// Quota failure is reported as unknown and does not invalidate a good login.
    pub async fn verify_login(&self, force: bool) -> Result<bool, Error> {
        if !force && !self.needs_refresh() {
            return self.readiness(false);
        }
        let mut cmd = Command::new(&self.command);
        self.isolate(&mut cmd);
        cmd.args(["app-server", "--stdio", "-c", "model_provider=\"openai\""])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let result = match cmd.spawn() {
            Err(_) => Err("Codex CLI could not be started for login verification".to_string()),
            Ok(mut child) => {
                let input = child.stdin.take().expect("piped input");
                let output = child.stdout.take().expect("piped output");
                let result = tokio::time::timeout(Duration::from_secs(20), probe(input, output))
                    .await
                    .unwrap_or_else(|_| Err("Codex CLI login verification timed out".into()));
                let _ = child.kill().await;
                result
            }
        };
        {
            let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            cache.checked = Some(Instant::now());
            match result {
                Ok(mut status) => {
                    // Native inference errors retain their cooldown despite an
                    // auth refresh; changing accounts clears the old observation.
                    if status.account_fingerprint == cache.status.account_fingerprint {
                        status.rate_limited_until = status
                            .rate_limited_until
                            .max(cache.status.rate_limited_until);
                    }
                    cache.status = status;
                }
                Err(why) => {
                    cache.status = CodexChatGptStatus {
                        error: Some(why),
                        ..Default::default()
                    };
                }
            }
        }
        self.readiness(true)
    }

    /// Only actual error envelopes count, never a model discussing usage limits.
    pub fn observe_quota(&self, output: &[u8]) -> bool {
        let limited = String::from_utf8_lossy(output).lines().any(|line| {
            serde_json::from_str::<Value>(line).is_ok_and(|event| {
                if event["type"] != "turn.failed" && event["type"] != "error" {
                    return false;
                }
                let code = event["error"]["codexErrorInfo"]
                    .as_str()
                    .unwrap_or_default();
                let text = event["error"]["message"]
                    .as_str()
                    .or_else(|| event["message"].as_str())
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                code == "usageLimitExceeded"
                    || text.contains("usage limit")
                    || text.contains("rate limit")
                    || text.contains("rate_limit")
                    || text.contains("hit your") && text.contains("limit")
                    || text.contains("usage_limit")
                    || text.contains("quota exceeded")
            })
        });
        if limited {
            let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            let until = Utc::now() + chrono::TimeDelta::seconds(BACKOFF_SECONDS);
            cache.status.rate_limited_until = Some(
                cache
                    .status
                    .rate_limited_until
                    .map_or(until, |old| old.max(until)),
            );
        }
        limited
    }
}

async fn rpc(
    input: &mut ChildStdin,
    output: &mut Lines<BufReader<ChildStdout>>,
    id: u64,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let mut request = serde_json::to_vec(&json!({"id":id,"method":method,"params":params}))
        .expect("JSON request");
    request.push(b'\n');
    input
        .write_all(&request)
        .await
        .map_err(|_| "Codex login probe could not write request")?;
    input
        .flush()
        .await
        .map_err(|_| "Codex login probe could not flush request")?;
    while let Some(line) = output
        .next_line()
        .await
        .map_err(|_| "Codex login probe could not read response")?
    {
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if event["id"] != id {
            continue;
        }
        // Never propagate arbitrary server errors containing account details.
        return event
            .get("result")
            .cloned()
            .ok_or_else(|| "Codex login probe request failed".into());
    }
    Err("Codex login probe exited without a response".into())
}

async fn probe(mut input: ChildStdin, output: ChildStdout) -> Result<CodexChatGptStatus, String> {
    let mut output = BufReader::new(output).lines();
    rpc(&mut input, &mut output, 1, "initialize", json!({"clientInfo":{"name":"rustykrab","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}})).await?;
    input
        .write_all(b"{\"method\":\"initialized\"}\n")
        .await
        .map_err(|_| "Codex initialization failed")?;
    let account = rpc(
        &mut input,
        &mut output,
        2,
        "account/read",
        json!({"refreshToken":false}),
    )
    .await?;
    let (subscription, fingerprint) = parse_account(&account)?;
    let mut status = CodexChatGptStatus {
        subscription: Some(subscription),
        account_fingerprint: Some(fingerprint),
        ..Default::default()
    };
    // Bound quota separately, so an unavailable quota API still exposes auth.
    match tokio::time::timeout(
        Duration::from_secs(5),
        rpc(
            &mut input,
            &mut output,
            3,
            "account/rateLimits/read",
            json!({}),
        ),
    )
    .await
    {
        Ok(Ok(quota)) => {
            status.rate_limits = parse_limits(&quota);
            status.quota_checked_at = Some(Utc::now());
            if quota["ordinaryUsageAllowed"] == false {
                let reset = status
                    .rate_limits
                    .get("codex")
                    .into_iter()
                    .flat_map(|b| [&b.primary, &b.secondary])
                    .flatten()
                    .filter(|w| w.used_percent >= 100.0)
                    .filter_map(|w| w.resets_at)
                    .filter_map(|at| DateTime::from_timestamp(at, 0))
                    .filter(|at| *at > Utc::now())
                    .max();
                status.rate_limited_until =
                    Some(reset.unwrap_or_else(|| {
                        Utc::now() + chrono::TimeDelta::seconds(BACKOFF_SECONDS)
                    }));
            }
        }
        _ => {
            status.quota_error =
                Some("Codex quota could not be checked; remaining capacity is unknown".into())
        }
    }
    Ok(status)
}

fn parse_account(result: &Value) -> Result<(String, String), String> {
    let account = &result["account"];
    if result["requiresOpenaiAuth"] != true || account["type"] != "chatgpt" {
        return Err("Selected Codex profile needs Sign in with ChatGPT; API billing and other providers are disabled".into());
    }
    let subscription = account["planType"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("Codex did not identify the ChatGPT plan")?;
    let email = account["email"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or("Codex did not identify the signed-in account")?;
    let digest = Sha256::digest(email.trim().to_ascii_lowercase().as_bytes());
    Ok((
        subscription.into(),
        format!("{:x}", digest)[..12].to_string(),
    ))
}

fn parse_limits(result: &Value) -> BTreeMap<String, CodexQuotaBucket> {
    fn window(value: &Value) -> Option<CodexQuotaWindow> {
        let used_percent = value["usedPercent"].as_f64()?;
        Some(CodexQuotaWindow {
            used_percent: used_percent.clamp(0.0, 100.0),
            window_minutes: value["windowDurationMins"].as_u64(),
            resets_at: value["resetsAt"].as_i64(),
        })
    }
    let bucket = |value: &Value| CodexQuotaBucket {
        primary: window(&value["primary"]),
        secondary: window(&value["secondary"]),
    };
    if let Some(map) = result["rateLimitsByLimitId"]
        .as_object()
        .filter(|m| !m.is_empty())
    {
        return map
            .iter()
            .map(|(id, value)| (id.clone(), bucket(value)))
            .collect();
    }
    let value = &result["rateLimits"];
    if value.is_object() {
        BTreeMap::from([(
            value["limitId"].as_str().unwrap_or("codex").into(),
            bucket(value),
        )])
    } else {
        BTreeMap::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chatgpt_identity_is_hashed_and_other_auth_is_refused() {
        let account = json!({"requiresOpenaiAuth":true,"account":{"type":"chatgpt","planType":"pro","email":" PERSON@example.invalid "}});
        let (plan, fingerprint) = parse_account(&account).unwrap();
        assert_eq!(plan, "pro");
        assert_eq!(fingerprint.len(), 12);
        let normalized = json!({"requiresOpenaiAuth":true,"account":{"type":"chatgpt","planType":"pro","email":"person@example.invalid"}});
        assert_eq!(parse_account(&normalized).unwrap().1, fingerprint);
        for auth in ["apiKey", "amazonBedrock", "chatgptAuthTokens"] {
            let mut bad = account.clone();
            bad["account"]["type"] = json!(auth);
            assert!(parse_account(&bad).is_err());
        }
        assert!(parse_account(&json!({"account":null})).is_err());
    }
    #[test]
    fn quotas_are_sanitized_and_missing_windows_stay_unknown() {
        let limits = parse_limits(
            &json!({"accountId":"private-id","rateLimitsByLimitId":{"codex":{"primary":{"usedPercent":23.5,"windowDurationMins":300,"resetsAt":2000000000},"secondary":null,"credits":{"balance":"private"}}}}),
        );
        let b = &limits["codex"];
        assert_eq!(b.primary.as_ref().unwrap().used_percent, 23.5);
        assert!(b.secondary.is_none());
        let serialized = serde_json::to_string(&limits).unwrap();
        assert!(!serialized.contains("private"));
        assert!(parse_limits(&json!({})).is_empty());
        assert!(
            parse_limits(&json!({"rateLimits":{"primary":{"usedPercent":null}}}))["codex"]
                .primary
                .is_none()
        );
    }
    #[test]
    fn quota_errors_are_typed_and_not_confused_with_agent_messages() {
        let r = CodexChatGptRuntime::new("codex".into(), None);
        assert!(!r.observe_quota(
            br#"{"type":"item.completed","item":{"type":"agent_message","text":"usage limit"}}"#
        ));
        assert!(
            r.observe_quota(br#"{"type":"turn.failed","error":{"message":"Usage limit reached"}}"#)
        );
        assert!(matches!(r.readiness(false), Err(Error::ModelRateLimit(_))));
        assert!(r.status().rate_limited_until.is_some());
    }
}
