//! Live browser-use evaluations against production websites.
//!
//! This suite answers a different question from the component-level ignored
//! tests in `rustykrab-tools`: can the real daemon, real model, browser tool,
//! credential request flow, and Chrome process complete a recognisable user
//! journey together? It is opt-in, never part of `--mode all`, and each trial
//! gets a fresh data directory, CDP port, and Chrome profile.
//!
//! The three initial journeys deliberately cover different failure surfaces:
//!
//! - Google Flights: dynamic controls, date picker, result selection.
//! - Instagram: authentication and bot-sensitive UI.
//! - United: authentication followed by a multi-step flight search.
//!
//! Run one or all configured cases with:
//!
//! ```sh
//! export RK_BROWSER_DEPART_DATE=2026-10-15
//! export RK_INSTAGRAM_USER=...
//! export RK_INSTAGRAM_PASS=...
//! export RK_INSTAGRAM_EXPECT=...
//! export RK_UNITED_USER=...
//! export RK_UNITED_PASS=...
//! export RK_UNITED_EXPECT=...
//! scripts/e2e.sh --mode browser --trials 1
//! ```
//!
//! `RK_BROWSER_ORIGIN` and `RK_BROWSER_DESTINATION` default to `SFO` and
//! `LAX`. A selected scenario fails (rather than silently skipping) when its
//! required live configuration is absent. Login credentials are inserted
//! through the same credential-request HTTP boundary used by a client and then
//! typed with `fill_credential`; they are never placed in an agent prompt, and
//! usernames and passwords are redacted from the evidence report.

use std::collections::BTreeSet;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::credential_suite::{credential_requests_filed, drive_gateway};
use crate::transcript::Transcript;
use crate::{
    keep_or_drop, kill_browser_for, pick_free_port, shutdown_daemon, spawn_daemon_with,
    wait_for_health, Backend, Expected, ScenarioReport, ALLOWED_ORIGIN,
};

const BROWSER_TOOLS: &[&str] = &["browser", "http_session"];
const ASK_WINDOW: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Copy)]
enum JourneyKind {
    GoogleFlights,
    Instagram,
    United,
}

pub struct BrowserScenario {
    pub id: &'static str,
    pub description: &'static str,
    kind: JourneyKind,
}

pub const SCENARIOS: &[BrowserScenario] = &[
    BrowserScenario {
        id: "google_flights_select_result",
        description: "Searches Google Flights and opens one outbound result without booking",
        kind: JourneyKind::GoogleFlights,
    },
    BrowserScenario {
        id: "instagram_login",
        description: "Requests credentials, signs in to Instagram, and observes the account",
        kind: JourneyKind::Instagram,
    },
    BrowserScenario {
        id: "united_login_and_select_flight",
        description: "Signs in to United, searches a route, and selects a flight without purchase",
        kind: JourneyKind::United,
    },
];

#[derive(Debug, Clone)]
struct Credentials {
    user: String,
    pass: String,
}

#[derive(Debug, Clone)]
struct JourneyConfig {
    url: &'static str,
    host: &'static str,
    prompt: String,
    resume_prompt: Option<String>,
    credentials: Option<Credentials>,
    account_marker: Option<String>,
    expected_text: Vec<String>,
    origin: Option<String>,
    destination: Option<String>,
    depart_date: Option<String>,
    min_applied_clicks: usize,
    min_route_inputs: usize,
    requires_flight_details: bool,
}

impl JourneyConfig {
    fn from_env(scenario: &BrowserScenario) -> std::result::Result<Self, Vec<&'static str>> {
        let nonempty = |key: &str| std::env::var(key).ok().filter(|v| !v.trim().is_empty());
        let origin = nonempty("RK_BROWSER_ORIGIN").unwrap_or_else(|| "SFO".to_string());
        let destination = nonempty("RK_BROWSER_DESTINATION").unwrap_or_else(|| "LAX".to_string());

        match scenario.kind {
            JourneyKind::GoogleFlights => {
                let Some(depart_date) = nonempty("RK_BROWSER_DEPART_DATE") else {
                    return Err(vec!["RK_BROWSER_DEPART_DATE"]);
                };
                Ok(Self {
                    url: "https://www.google.com/travel/flights",
                    host: "google.com",
                    prompt: format!(
                        "Use the browser tool to open Google Flights and search for a one-way flight from {origin} to {destination} on {depart_date}. Enter the itinerary through the on-page controls; do not encode it in a navigation URL. Take one snapshot with compact=true and interactive=true, then act on its refs. Every act returns fresh page_state: use its refs directly instead of taking a duplicate snapshot. If a needed control is absent, scroll once and resnapshot; use interactive=false only when visible result text is needed. Select one specific outbound flight result so its visible details are open, then stop before continuing to an airline or booking page. Report the exact visible airline, departure time, and price. Do not claim success unless you observed those details in the browser."
                    ),
                    resume_prompt: None,
                    credentials: None,
                    account_marker: None,
                    expected_text: vec![origin.clone(), destination.clone()],
                    origin: Some(origin),
                    destination: Some(destination),
                    depart_date: Some(depart_date),
                    min_applied_clicks: 4,
                    min_route_inputs: 2,
                    requires_flight_details: true,
                })
            }
            JourneyKind::Instagram => {
                let mut missing = Vec::new();
                let user = nonempty("RK_INSTAGRAM_USER").unwrap_or_else(|| {
                    missing.push("RK_INSTAGRAM_USER");
                    String::new()
                });
                let pass = nonempty("RK_INSTAGRAM_PASS").unwrap_or_else(|| {
                    missing.push("RK_INSTAGRAM_PASS");
                    String::new()
                });
                let expect = nonempty("RK_INSTAGRAM_EXPECT").unwrap_or_else(|| {
                    missing.push("RK_INSTAGRAM_EXPECT");
                    String::new()
                });
                if !missing.is_empty() {
                    return Err(missing);
                }
                Ok(Self {
                    url: "https://www.instagram.com/accounts/login/",
                    host: "instagram.com",
                    prompt: "Use the browser tool to open Instagram and sign in. Take one snapshot with compact=true and interactive=true, then act on its refs. Every act returns fresh page_state: use its refs directly instead of taking a duplicate snapshot. If a needed control is absent, scroll once and resnapshot; use interactive=false only to verify visible account text. If credentials are missing, request username and password through the secure credential flow. Never ask me to paste a password into chat. Once credentials are available, use browser fill_credential for both fields and complete sign-in. Prove authenticated access by reporting the exact account handle or account text visible after login. Do not like, follow, message, post, or change account settings."
                        .to_string(),
                    resume_prompt: Some(
                        "The requested Instagram credentials have been supplied securely. Continue the existing task with fresh browser state. Use fill_credential rather than asking for or echoing either value."
                            .to_string(),
                    ),
                    credentials: Some(Credentials { user, pass }),
                    account_marker: Some(expect.clone()),
                    expected_text: vec![expect],
                    origin: None,
                    destination: None,
                    depart_date: None,
                    min_applied_clicks: 1,
                    min_route_inputs: 0,
                    requires_flight_details: false,
                })
            }
            JourneyKind::United => {
                let mut missing = Vec::new();
                let user = nonempty("RK_UNITED_USER").unwrap_or_else(|| {
                    missing.push("RK_UNITED_USER");
                    String::new()
                });
                let pass = nonempty("RK_UNITED_PASS").unwrap_or_else(|| {
                    missing.push("RK_UNITED_PASS");
                    String::new()
                });
                let depart_date = nonempty("RK_BROWSER_DEPART_DATE").unwrap_or_else(|| {
                    missing.push("RK_BROWSER_DEPART_DATE");
                    String::new()
                });
                let expect = nonempty("RK_UNITED_EXPECT").unwrap_or_else(|| {
                    missing.push("RK_UNITED_EXPECT");
                    String::new()
                });
                if !missing.is_empty() {
                    return Err(missing);
                }
                Ok(Self {
                    url: "https://www.united.com/",
                    host: "united.com",
                    prompt: format!(
                        "Use the browser tool to open United Airlines and sign in. Take one snapshot with compact=true and interactive=true, then act on its refs. Every act returns fresh page_state: use its refs directly instead of taking a duplicate snapshot. If a needed control is absent, scroll once and resnapshot; use interactive=false only when visible account or flight text is needed. If credentials are missing, request username and password through the secure credential flow; never ask me to paste a password into chat. After sign-in, search for a one-way flight from {origin} to {destination} on {depart_date}, entering the itinerary through the on-page controls rather than encoding it in a navigation URL. Select one specific outbound flight so its details are visible, then stop before choosing a fare, entering traveler information, or purchasing anything. Report the visible signed-in account marker and the exact selected flight time and price."
                    ),
                    resume_prompt: Some(format!(
                        "The requested United credentials have been supplied securely. Continue the existing task: sign in with fill_credential, search {origin} to {destination} on {depart_date}, select one outbound flight, and stop before fare selection or purchase."
                    )),
                    credentials: Some(Credentials { user, pass }),
                    account_marker: Some(expect.clone()),
                    expected_text: vec![expect, origin.clone(), destination.clone()],
                    origin: Some(origin),
                    destination: Some(destination),
                    depart_date: Some(depart_date),
                    min_applied_clicks: 3,
                    min_route_inputs: 2,
                    requires_flight_details: true,
                })
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserJourneyOutcome {
    Succeeded,
    JourneyFailed,
    NeverAsked,
    Timeout,
    Error,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BrowserEvidence {
    pub browser_calls: usize,
    pub navigation_calls: usize,
    pub snapshot_calls: usize,
    pub applied_actions: usize,
    pub applied_clicks: usize,
    pub resolved_action_attempts: usize,
    pub route_inputs: usize,
    pub credential_fills: usize,
    pub post_login_marker_observed: bool,
    pub unconfirmed_unknown_actions: usize,
    pub failed_browser_calls: usize,
    pub requested_hosts: Vec<String>,
    pub browser_output_contained_route: bool,
    pub browser_output_contained_depart_date: bool,
    pub browser_output_contained_flight_details: bool,
    pub model_calls: usize,
    pub max_prompt_tokens: u64,
    pub max_model_call_ms: u64,
    pub compaction_events: usize,
    pub cdp_schema_drift_events: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserJourneyTrial {
    pub scenario: String,
    pub trial: usize,
    pub outcome: BrowserJourneyOutcome,
    pub requests_filed: i64,
    pub elapsed_secs: f64,
    pub evidence: BrowserEvidence,
    pub reply: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub async fn run(
    bin: &str,
    model: &str,
    ollama_url: &str,
    trials: usize,
    case_filter: Option<&str>,
    timeout: Duration,
) -> Result<(Vec<ScenarioReport>, Vec<BrowserJourneyTrial>)> {
    let selected: Vec<&BrowserScenario> = SCENARIOS
        .iter()
        .filter(|scenario| case_filter.is_none_or(|needle| scenario.id.contains(needle)))
        .collect();
    if selected.is_empty() {
        anyhow::bail!(
            "no browser scenario matched --case {}",
            case_filter.unwrap_or("<none>")
        );
    }
    let mut reports = Vec::new();
    let mut all_trials = Vec::new();

    for scenario in selected {
        let config = match JourneyConfig::from_env(scenario) {
            Ok(config) => config,
            Err(missing) => {
                let detail = format!(
                    "missing required live-browser configuration: {}",
                    missing.join(", ")
                );
                eprintln!(
                    "  {} not run: {} (browser mode is explicit, so this is a failed evaluation)",
                    scenario.id, detail
                );
                reports.push(ScenarioReport::new(
                    scenario.id,
                    "browser",
                    Expected::Pass,
                    trials,
                    0,
                    vec![detail.clone()],
                    0,
                ));
                all_trials.push(BrowserJourneyTrial {
                    scenario: scenario.id.to_string(),
                    trial: 0,
                    outcome: BrowserJourneyOutcome::Error,
                    requests_filed: 0,
                    elapsed_secs: 0.0,
                    evidence: BrowserEvidence::default(),
                    reply: String::new(),
                    error: Some(detail),
                });
                continue;
            }
        };

        eprintln!(
            "browser suite: {} x {trials} trial(s) against {}",
            scenario.id, config.url
        );
        let started = Instant::now();
        let mut passes = 0;
        let mut details = Vec::new();
        for trial in 1..=trials {
            let result = run_trial(bin, model, ollama_url, scenario, &config, trial, timeout).await;
            eprintln!(
                "  {} #{trial} -> {:?} ({:.0}s)",
                scenario.id, result.outcome, result.elapsed_secs
            );
            if result.outcome == BrowserJourneyOutcome::Succeeded {
                passes += 1;
            } else {
                let detail = result
                    .error
                    .clone()
                    .unwrap_or_else(|| format!("{:?}", result.outcome));
                if !details.contains(&detail) {
                    details.push(detail);
                }
            }
            all_trials.push(result);
        }
        reports.push(ScenarioReport::new(
            scenario.id,
            "browser",
            Expected::Pass,
            trials,
            passes,
            details,
            started.elapsed().as_millis() / trials.max(1) as u128,
        ));
    }

    Ok((reports, all_trials))
}

#[allow(clippy::too_many_arguments)]
async fn run_trial(
    bin: &str,
    model: &str,
    ollama_url: &str,
    scenario: &BrowserScenario,
    config: &JourneyConfig,
    trial: usize,
    timeout: Duration,
) -> BrowserJourneyTrial {
    let started = Instant::now();
    let mut result = BrowserJourneyTrial {
        scenario: scenario.id.to_string(),
        trial,
        outcome: BrowserJourneyOutcome::Error,
        requests_filed: 0,
        elapsed_secs: 0.0,
        evidence: BrowserEvidence::default(),
        reply: String::new(),
        error: None,
    };

    let tmp = match tempfile::Builder::new()
        .prefix("rustykrab-e2e-browser-")
        .tempdir()
    {
        Ok(tmp) => tmp,
        Err(error) => {
            result.error = Some(format!("harness tempdir: {error}"));
            result.elapsed_secs = started.elapsed().as_secs_f64();
            return result;
        }
    };
    let data_dir = tmp.path().to_path_buf();
    let port = match pick_free_port() {
        Ok(port) => port,
        Err(error) => {
            result.error = Some(format!("harness port: {error}"));
            result.elapsed_secs = started.elapsed().as_secs_f64();
            keep_or_drop(tmp);
            return result;
        }
    };

    let mut extra_env = Vec::new();
    if let Ok(allow_hosts) = std::env::var("RUSTYKRAB_SSRF_ALLOW_HOSTS") {
        if !allow_hosts.trim().is_empty() {
            extra_env.push(("RUSTYKRAB_SSRF_ALLOW_HOSTS".to_string(), allow_hosts));
        }
    }
    if let Ok(cdp_port) = pick_free_port() {
        extra_env.push(("CHROME_CDP_PORT".to_string(), cdp_port.to_string()));
    }

    let backend = Backend::Model {
        model,
        ollama_url,
        num_ctx: None,
        active_tools: BROWSER_TOOLS,
        tool_stubs: "",
        channel: None,
        extra_env: &extra_env,
    };
    let mut child = match spawn_daemon_with(bin, &data_dir, port, &backend) {
        Ok(child) => child,
        Err(error) => {
            result.error = Some(format!("harness spawn: {error:#}"));
            result.elapsed_secs = started.elapsed().as_secs_f64();
            keep_or_drop(tmp);
            return result;
        }
    };

    let run = tokio::time::timeout(
        timeout,
        run_trial_inner(&data_dir, port, &mut child, config, &mut result, timeout),
    )
    .await;
    match run {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            result.outcome = BrowserJourneyOutcome::Error;
            result.error = Some(format!("harness error: {error:#}"));
        }
        Err(_) => {
            result.outcome = BrowserJourneyOutcome::Timeout;
            result.error = Some(format!("trial exceeded {}s", timeout.as_secs()));
            let log = crate::log_tail(&data_dir);
            eprintln!(
                "--- daemon.log tail (trial {trial}) ---\n{}",
                redact(&log, config)
            );
        }
    }

    shutdown_daemon(child).await;
    kill_browser_for(&data_dir);
    let log = std::fs::read_to_string(data_dir.join("daemon.log")).unwrap_or_default();
    merge_runtime_log_evidence(&log, &mut result.evidence, config);
    keep_or_drop(tmp);
    result.elapsed_secs = started.elapsed().as_secs_f64();
    result
}

/// Merge process-boundary evidence that remains available even when the HTTP
/// request is cancelled on timeout and the unfinished turn was never saved.
fn merge_runtime_log_evidence(log: &str, evidence: &mut BrowserEvidence, config: &JourneyConfig) {
    let log = strip_ansi(log);
    evidence.browser_calls = evidence
        .browser_calls
        .max(log.matches("tool call started tool=browser").count());
    evidence.failed_browser_calls = evidence
        .failed_browser_calls
        .max(log.matches("tool call failed tool=browser").count());
    evidence.snapshot_calls = evidence
        .snapshot_calls
        .max(log.matches("took page snapshot").count());
    evidence.resolved_action_attempts = log.matches("browser action resolved element").count();
    evidence.applied_actions = evidence.applied_actions.max(
        log.lines()
            .filter(|line| {
                line.contains("browser action completed") && line.contains("outcome=\"applied\"")
            })
            .count(),
    );
    evidence.applied_clicks = evidence.applied_clicks.max(
        log.lines()
            .filter(|line| {
                line.contains("browser action completed")
                    && line.contains("action=\"click\"")
                    && line.contains("outcome=\"applied\"")
            })
            .count(),
    );
    evidence.credential_fills = evidence.credential_fills.max(
        log.lines()
            .filter(|line| {
                line.contains("browser credential fill completed")
                    && line.contains("outcome=\"applied\"")
            })
            .count(),
    );
    if config.origin.is_some() {
        let applied_text_inputs = log
            .lines()
            .filter(|line| {
                line.contains("browser action completed")
                    && matches!(
                        metric_string(line, "action").as_deref(),
                        Some("type" | "fill")
                    )
                    && line.contains("outcome=\"applied\"")
            })
            .count();
        evidence.route_inputs = evidence
            .route_inputs
            .max(applied_text_inputs.saturating_sub(evidence.credential_fills));
    }
    evidence.model_calls = log.matches("LLM call completed").count();
    evidence.compaction_events = log
        .matches("conversation crossed compaction threshold")
        .count();
    evidence.max_prompt_tokens = metric_max(&log, "LLM call completed", "prompt_tokens");
    evidence.max_model_call_ms = metric_max(&log, "LLM call completed", "duration_ms");
    evidence.cdp_schema_drift_events = log
        .lines()
        .filter(|line| line.contains("tolerated known CDP schema drift"))
        .filter_map(|line| metric(line, "count"))
        .max()
        .unwrap_or(0);
    if log
        .lines()
        .any(|line| line.contains("took page snapshot") && line.contains(config.host))
    {
        evidence.navigation_calls = evidence.navigation_calls.max(1);
        if !evidence
            .requested_hosts
            .iter()
            .any(|host| host == config.host)
        {
            evidence.requested_hosts.push(config.host.to_string());
        }
    }
    if config
        .depart_date
        .as_deref()
        .is_some_and(|date| contains_date(&log.to_ascii_lowercase(), date))
    {
        evidence.browser_output_contained_depart_date = true;
    }
}

fn metric_max(log: &str, line_marker: &str, key: &str) -> u64 {
    log.lines()
        .filter(|line| line.contains(line_marker))
        .filter_map(|line| metric(line, key))
        .max()
        .unwrap_or(0)
}

fn metric(line: &str, key: &str) -> Option<u64> {
    let tail = line.get(line.find(key)? + key.len()..)?.trim_start();
    let digits = tail.strip_prefix('=')?.trim_start();
    let end = digits
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(digits.len());
    digits[..end].parse().ok()
}

fn metric_string(line: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=");
    let value = line
        .get(line.rfind(&needle)? + needle.len()..)?
        .trim_start();
    if let Some(quoted) = value.strip_prefix('"') {
        return Some(quoted.get(..quoted.find('"')?)?.to_string());
    }
    Some(
        value
            .get(..value.find(char::is_whitespace).unwrap_or(value.len()))?
            .to_string(),
    )
}

fn strip_ansi(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(character) = chars.next() {
        if character == '\u{1b}' && chars.next() == Some('[') {
            for control in chars.by_ref() {
                if ('@'..='~').contains(&control) {
                    break;
                }
            }
        } else {
            output.push(character);
        }
    }
    output
}

async fn run_trial_inner(
    data_dir: &std::path::Path,
    port: u16,
    child: &mut std::process::Child,
    config: &JourneyConfig,
    result: &mut BrowserJourneyTrial,
    timeout: Duration,
) -> Result<()> {
    let base = format!("http://127.0.0.1:{port}");
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::ORIGIN,
        reqwest::header::HeaderValue::from_static(ALLOWED_ORIGIN),
    );
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .default_headers(headers)
        .build()?;
    wait_for_health(&base, &client, child).await?;

    let db_path = data_dir.join("db").join("store.db");
    let mut conversation = None;
    let first = drive_gateway(&base, &client, &config.prompt, &mut conversation).await?;
    let mut raw_reply = first.text;
    result.reply = redact(&raw_reply, config);

    if let Some(credentials) = &config.credentials {
        result.requests_filed = credential_requests_filed(&db_path);
        if !fulfil_pending(&base, &client, credentials).await? {
            result.outcome = BrowserJourneyOutcome::NeverAsked;
            result.error = Some("agent never filed a credential request".to_string());
            return Ok(());
        }
        let second = drive_gateway(
            &base,
            &client,
            config.resume_prompt.as_deref().unwrap_or(
                "The requested credentials have been supplied securely. Continue the task.",
            ),
            &mut conversation,
        )
        .await?;
        raw_reply = format!("{}\n---\n{}", raw_reply.trim(), second.text.trim());
        result.reply = redact(&raw_reply, config);
    }

    let conversation = conversation.context("gateway did not return a conversation id")?;
    let transcript = Transcript::from_store(&db_path, &conversation)?;
    result.evidence = collect_evidence(&transcript, config);
    // Grade the private in-memory reply, then retain only the redacted form in
    // the serializable trial record. Otherwise an expected account marker that
    // equals the username is redacted before grading and can never pass.
    result.outcome = assess(&raw_reply, &result.evidence, config);
    if result.outcome != BrowserJourneyOutcome::Succeeded {
        result.error = Some(failure_summary(&raw_reply, &result.evidence, config));
    }
    Ok(())
}

fn collect_evidence(transcript: &Transcript, config: &JourneyConfig) -> BrowserEvidence {
    let calls = transcript.calls_to("browser");
    let mut evidence = BrowserEvidence {
        browser_calls: calls.len(),
        ..BrowserEvidence::default()
    };
    let mut hosts = BTreeSet::new();
    let route_needles: Vec<String> = [config.origin.as_deref(), config.destination.as_deref()]
        .into_iter()
        .flatten()
        .map(|value| value.to_ascii_lowercase())
        .collect();

    for call in calls {
        let action = call.args["action"].as_str().unwrap_or_default();
        let subaction = call.args["actAction"].as_str().unwrap_or_default();
        let output = call.output.as_ref();
        let outcome = output
            .and_then(|value| value["outcome"].as_str())
            .unwrap_or_default();
        let confirmed = output
            .and_then(|value| value["confirmed_by"].as_str())
            .is_some();

        if call.failed {
            evidence.failed_browser_calls += 1;
        }
        if action == "open" || action == "navigate" {
            evidence.navigation_calls += 1;
            if call.args["url"]
                .as_str()
                .is_some_and(|url| url.contains(config.host))
            {
                hosts.insert(config.host.to_string());
            }
        }
        if action == "snapshot" {
            evidence.snapshot_calls += 1;
        }
        if outcome == "applied" {
            evidence.applied_actions += 1;
        }
        if action == "act" && subaction == "click" && outcome == "applied" {
            evidence.applied_clicks += 1;
        }
        if (action == "fill_credential" || (action == "act" && subaction == "fill_credential"))
            && outcome == "applied"
        {
            evidence.credential_fills += 1;
        }
        if action == "act" && matches!(subaction, "type" | "fill") {
            let text = call.args["text"]
                .as_str()
                .unwrap_or_default()
                .to_ascii_lowercase();
            if config
                .origin
                .as_ref()
                .is_some_and(|value| text.contains(&value.to_ascii_lowercase()))
                || config
                    .destination
                    .as_ref()
                    .is_some_and(|value| text.contains(&value.to_ascii_lowercase()))
            {
                evidence.route_inputs += 1;
            }
        }
        if outcome == "unknown" && !confirmed {
            evidence.unconfirmed_unknown_actions += 1;
        }
        if let Some(output) = output {
            let rendered = output.to_string().to_ascii_lowercase();
            if evidence.credential_fills >= 2
                && evidence.applied_clicks >= 1
                && config
                    .account_marker
                    .as_ref()
                    .is_some_and(|marker| rendered.contains(&marker.to_ascii_lowercase()))
            {
                evidence.post_login_marker_observed = true;
            }
            if !route_needles.is_empty()
                && route_needles.iter().all(|needle| rendered.contains(needle))
            {
                evidence.browser_output_contained_route = true;
            }
            if config
                .depart_date
                .as_deref()
                .is_some_and(|date| contains_date(&rendered, date))
            {
                evidence.browser_output_contained_depart_date = true;
            }
            if evidence.route_inputs >= config.min_route_inputs
                && evidence.applied_clicks >= config.min_applied_clicks
                && contains_price_and_time(&rendered)
            {
                evidence.browser_output_contained_flight_details = true;
            }
        }
    }
    evidence.requested_hosts = hosts.into_iter().collect();
    evidence
}

fn assess(
    reply: &str,
    evidence: &BrowserEvidence,
    config: &JourneyConfig,
) -> BrowserJourneyOutcome {
    let reply_lower = reply.to_ascii_lowercase();
    let expected_visible = config
        .expected_text
        .iter()
        .all(|text| reply_lower.contains(&text.to_ascii_lowercase()));
    let credential_boundary = config
        .credentials
        .as_ref()
        .is_none_or(|_| evidence.credential_fills >= 2);
    let authenticated_boundary = config
        .account_marker
        .as_ref()
        .is_none_or(|_| evidence.post_login_marker_observed);
    let route_boundary = config.origin.is_none()
        || (evidence.route_inputs >= config.min_route_inputs
            && evidence.browser_output_contained_route);
    let date_boundary =
        config.depart_date.is_none() || evidence.browser_output_contained_depart_date;
    let flight_boundary =
        !config.requires_flight_details || evidence.browser_output_contained_flight_details;

    if expected_visible
        && evidence.navigation_calls > 0
        && evidence.snapshot_calls > 0
        && evidence.applied_clicks >= config.min_applied_clicks
        && credential_boundary
        && authenticated_boundary
        && route_boundary
        && date_boundary
        && flight_boundary
        && evidence.unconfirmed_unknown_actions == 0
    {
        BrowserJourneyOutcome::Succeeded
    } else {
        BrowserJourneyOutcome::JourneyFailed
    }
}

fn failure_summary(reply: &str, evidence: &BrowserEvidence, config: &JourneyConfig) -> String {
    let reply_lower = reply.to_ascii_lowercase();
    let missing_expected_count = config
        .expected_text
        .iter()
        .filter(|text| !reply_lower.contains(&text.to_ascii_lowercase()))
        .count();
    format!(
        "journey evidence incomplete: missing_expected_count={missing_expected_count}, navigation={}, snapshots={}, applied_clicks={}/{}, route_inputs={}/{}, route_observed={}, date_observed={}, flight_details_observed={}, credential_fills={}, post_login_marker_observed={}, unknown_actions={}, failed_browser_calls={}",
        evidence.navigation_calls,
        evidence.snapshot_calls,
        evidence.applied_clicks,
        config.min_applied_clicks,
        evidence.route_inputs,
        config.min_route_inputs,
        evidence.browser_output_contained_route,
        evidence.browser_output_contained_depart_date,
        evidence.browser_output_contained_flight_details,
        evidence.credential_fills,
        evidence.post_login_marker_observed,
        evidence.unconfirmed_unknown_actions,
        evidence.failed_browser_calls,
    )
}

/// Sites render an ISO input date in several locale-neutral English forms.
/// Accept those display forms while still requiring the requested year/day;
/// this keeps the evaluator independent of whether the model typed into a
/// date field or used a calendar picker.
fn contains_date(rendered_lower: &str, iso_date: &str) -> bool {
    let parts: Vec<&str> = iso_date.split('-').collect();
    let [year, month, day] = parts.as_slice() else {
        return rendered_lower.contains(&iso_date.to_ascii_lowercase());
    };
    let Ok(month_number) = month.parse::<usize>() else {
        return rendered_lower.contains(&iso_date.to_ascii_lowercase());
    };
    let Ok(day_number) = day.parse::<u32>() else {
        return rendered_lower.contains(&iso_date.to_ascii_lowercase());
    };
    let month_names = [
        ("jan", "january"),
        ("feb", "february"),
        ("mar", "march"),
        ("apr", "april"),
        ("may", "may"),
        ("jun", "june"),
        ("jul", "july"),
        ("aug", "august"),
        ("sep", "september"),
        ("oct", "october"),
        ("nov", "november"),
        ("dec", "december"),
    ];
    let Some((short, long)) = month_names.get(month_number.saturating_sub(1)) else {
        return false;
    };
    let numeric = format!("{month_number}/{day_number}/{year}");
    let numeric_short = format!("{month_number}/{day_number}");
    let short_text = format!("{short} {day_number}");
    let long_text = format!("{long} {day_number}");
    rendered_lower.contains(&iso_date.to_ascii_lowercase())
        || rendered_lower.contains(&numeric)
        || rendered_lower.contains(&numeric_short)
        || rendered_lower.contains(&short_text)
        || rendered_lower.contains(&long_text)
}

fn contains_price_and_time(rendered: &str) -> bool {
    static PRICE: OnceLock<regex::Regex> = OnceLock::new();
    static TIME: OnceLock<regex::Regex> = OnceLock::new();
    let rendered_lower = rendered.to_ascii_lowercase();
    let has_price = PRICE
        .get_or_init(|| {
            regex::Regex::new(r"(?:\$\s?\d|\b(?:usd|dollars?)\b)").expect("static price regex")
        })
        .is_match(&rendered_lower);
    let has_time = TIME
        .get_or_init(|| {
            regex::Regex::new(r"\b(?:[01]?\d|2[0-3]):[0-5]\d\s*(?:am|pm)?\b")
                .expect("static time regex")
        })
        .is_match(&rendered_lower);
    has_price && has_time
}

async fn fulfil_pending(
    base: &str,
    client: &reqwest::Client,
    credentials: &Credentials,
) -> Result<bool> {
    let deadline = Instant::now() + ASK_WINDOW;
    loop {
        let pending: Vec<Value> = client
            .get(format!("{base}/api/credential-requests"))
            .bearer_auth(crate::AUTH_TOKEN)
            .send()
            .await?
            .json()
            .await
            .context("listing credential requests")?;

        if !pending.is_empty() {
            for request in &pending {
                let Some(id) = request["id"].as_str() else {
                    continue;
                };
                let mut values = serde_json::Map::new();
                for field in request["fields"].as_array().unwrap_or(&Vec::new()) {
                    let Some(key) = field["key"].as_str() else {
                        continue;
                    };
                    let value = if field["secret"].as_bool().unwrap_or(true) {
                        &credentials.pass
                    } else {
                        &credentials.user
                    };
                    values.insert(key.to_string(), json!(value));
                }
                if !values.is_empty() {
                    client
                        .post(format!("{base}/api/credential-requests/{id}/fulfil"))
                        .bearer_auth(crate::AUTH_TOKEN)
                        .json(&json!({"values": values}))
                        .send()
                        .await?
                        .error_for_status()?;
                }
            }
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn redact(text: &str, config: &JourneyConfig) -> String {
    match &config.credentials {
        Some(credentials) => text
            .replace(&credentials.pass, "[redacted-password]")
            .replace(&credentials.user, "[redacted-username]"),
        _ => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn google_config() -> JourneyConfig {
        JourneyConfig {
            url: "https://www.google.com/travel/flights",
            host: "google.com",
            prompt: String::new(),
            resume_prompt: None,
            credentials: None,
            account_marker: None,
            expected_text: vec!["SFO".into(), "LAX".into()],
            origin: Some("SFO".into()),
            destination: Some("LAX".into()),
            depart_date: Some("2026-10-15".into()),
            min_applied_clicks: 2,
            min_route_inputs: 2,
            requires_flight_details: true,
        }
    }

    #[test]
    fn runtime_log_evidence_survives_an_unsaved_timeout() {
        let log = concat!(
            "LLM call completed duration_ms=1200 prompt_tokens=10000\n",
            "tool call started tool=browser call_id=one\n",
            "tool call failed tool=browser call_id=two\n",
            "LLM call completed duration_ms=9300 prompt_tokens=24000\n",
            "conversation crossed compaction threshold\n",
            "tolerated known CDP schema drift count=64\n",
            "took page snapshot url=https://www.google.com/travel/flights?date=2026-10-15 chars=7000 nodes=170\n",
            "browser action resolved element action=click ref_id=s1-2\n",
            "browser action completed action=\"click\" ref_id=\"s1-2\" outcome=\"applied\" stage=\"verified\"\n",
            "browser action completed action=\"type\" ref_id=\"s1-3\" outcome=\"applied\" stage=\"verified\"\n",
            "browser action completed action=\"fill\" ref_id=\"s1-4\" outcome=\"applied\" stage=\"verified\"\n",
            "browser credential fill completed field=\"username\" outcome=\"applied\" stage=\"verified\"\n",
        );
        let mut evidence = BrowserEvidence::default();
        merge_runtime_log_evidence(log, &mut evidence, &google_config());
        assert_eq!(evidence.browser_calls, 1);
        assert_eq!(evidence.failed_browser_calls, 1);
        assert_eq!(evidence.model_calls, 2);
        assert_eq!(evidence.max_prompt_tokens, 24_000);
        assert_eq!(evidence.max_model_call_ms, 9_300);
        assert_eq!(evidence.compaction_events, 1);
        assert_eq!(evidence.cdp_schema_drift_events, 64);
        assert_eq!(evidence.snapshot_calls, 1);
        assert_eq!(evidence.resolved_action_attempts, 1);
        assert_eq!(evidence.applied_actions, 3);
        assert_eq!(evidence.applied_clicks, 1);
        assert_eq!(evidence.route_inputs, 1);
        assert_eq!(evidence.credential_fills, 1);
        assert_eq!(evidence.navigation_calls, 1);
        assert_eq!(evidence.requested_hosts, vec!["google.com"]);
        assert!(evidence.browser_output_contained_depart_date);
    }

    #[test]
    fn runtime_log_parser_removes_terminal_colour_sequences() {
        let mut evidence = BrowserEvidence::default();
        merge_runtime_log_evidence(
            "\u{1b}[32mLLM call completed\u{1b}[0m prompt_tokens\u{1b}[0m=321",
            &mut evidence,
            &google_config(),
        );
        assert_eq!(evidence.model_calls, 1);
        assert_eq!(evidence.max_prompt_tokens, 321);
    }

    #[test]
    fn evidence_and_visible_result_are_both_required() {
        let config = google_config();
        let good = BrowserEvidence {
            navigation_calls: 1,
            snapshot_calls: 3,
            applied_clicks: 2,
            route_inputs: 2,
            browser_output_contained_route: true,
            browser_output_contained_depart_date: true,
            browser_output_contained_flight_details: true,
            ..BrowserEvidence::default()
        };
        assert_eq!(
            assess("SFO to LAX on 2026-10-15", &good, &config),
            BrowserJourneyOutcome::Succeeded
        );
        assert_eq!(
            assess(
                "SFO to LAX on 2026-10-15",
                &BrowserEvidence::default(),
                &config
            ),
            BrowserJourneyOutcome::JourneyFailed
        );
        assert_eq!(
            assess("I selected a flight", &good, &config),
            BrowserJourneyOutcome::JourneyFailed
        );
    }

    #[test]
    fn an_ambiguous_action_prevents_a_false_pass() {
        let config = google_config();
        let evidence = BrowserEvidence {
            navigation_calls: 1,
            snapshot_calls: 2,
            applied_clicks: 2,
            route_inputs: 2,
            browser_output_contained_route: true,
            browser_output_contained_depart_date: true,
            browser_output_contained_flight_details: true,
            unconfirmed_unknown_actions: 1,
            ..BrowserEvidence::default()
        };
        assert_eq!(
            assess("SFO LAX 2026-10-15", &evidence, &config),
            BrowserJourneyOutcome::JourneyFailed
        );
    }

    #[test]
    fn report_redaction_removes_login_values() {
        let mut config = google_config();
        config.credentials = Some(Credentials {
            user: "agent@example.com".into(),
            pass: "not-for-artifacts".into(),
        });
        let safe = redact("user=agent@example.com password=not-for-artifacts", &config);
        assert_eq!(
            safe,
            "user=[redacted-username] password=[redacted-password]"
        );
        let summary = failure_summary("", &BrowserEvidence::default(), &config);
        assert!(!summary.contains("agent@example.com"));
        assert!(!summary.contains("not-for-artifacts"));
    }

    #[test]
    fn date_evidence_accepts_common_site_renderings() {
        assert!(contains_date("selected: 2026-10-15", "2026-10-15"));
        assert!(contains_date("thu, oct 15, 2026", "2026-10-15"));
        assert!(contains_date("departing thu, oct 15", "2026-10-15"));
        assert!(contains_date("october 15 2026", "2026-10-15"));
        assert!(contains_date("10/15/2026", "2026-10-15"));
        assert!(!contains_date("oct 16, 2026", "2026-10-15"));
    }

    #[test]
    fn flight_details_require_a_visible_price_and_time() {
        assert!(contains_price_and_time("United 415 8:35 am $219"));
        assert!(contains_price_and_time("18:05 · USD 321"));
        assert!(!contains_price_and_time("from $219"));
        assert!(!contains_price_and_time("departs at 8:35 am"));
    }
}
