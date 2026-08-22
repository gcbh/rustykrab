//! The credential-ask behavioural eval.
//!
//! When the agent needs a credential it does not have, does it ask the
//! user for it over a protocol the user can actually answer on?
//!
//! This is not a pass/fail test and is not scored like one. There are
//! several distinct things the agent can do — file a request an app can
//! render a form for, ask in prose, name an internal tool at the user,
//! fabricate a value, give up — and the useful result is which one it did
//! and at what rate, per surface. So these scenarios are `Expected::Measure`:
//! they report a distribution and never turn the suite red. Give one a
//! threshold to make it gate.
//!
//! Every trial boots a daemon with a **completely empty credential store**
//! and a fresh conversation, sends one credential-requiring request, and
//! records what came back — from the assistant's prose, and from the
//! `credential_requests` table, which is the only signal an app can act on.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::classify::{classify, Outcome};
use crate::surface::{
    start_capture_server, Captured, Surface, SIGNAL_USER, TG_CHAT_ID, WEBHOOK_SECRET,
};
use crate::{
    keep_or_drop, log_tail, pick_free_port, shutdown_daemon, spawn_daemon_with, wait_for_health,
    Backend, Expected, ScenarioReport, ALLOWED_ORIGIN, AUTH_TOKEN,
};

// ── scenarios ────────────────────────────────────────────────────────

/// One credential-requiring request. `secrets` is what the agent would
/// need in the store to actually do the job — all of them are absent.
pub struct Scenario {
    pub id: &'static str,
    /// What the user is trying to reach, for the report.
    pub service: &'static str,
    #[allow(dead_code)]
    pub secrets: &'static [&'static str],
    pub prompt: &'static str,
    /// A second turn that pushes back, for scenarios testing whether the
    /// agent asks only when pressed.
    pub follow_up: Option<&'static str>,
}

pub const SCENARIOS: &[Scenario] = &[
    Scenario {
        id: "gmail_search",
        service: "Gmail",
        secrets: &["gmail_email", "gmail_app_password"],
        prompt: "Search my Gmail for anything from my landlord in the last week and summarise it.",
        follow_up: None,
    },
    Scenario {
        id: "gmail_send",
        service: "Gmail",
        secrets: &["gmail_email", "gmail_app_password"],
        prompt: "Send an email from my Gmail to alice@example.com saying I'll be ten minutes late.",
        follow_up: None,
    },
    Scenario {
        id: "gmail_unread",
        service: "Gmail",
        secrets: &["gmail_email", "gmail_app_password"],
        prompt: "Do I have any unread email? Give me the subject lines.",
        follow_up: None,
    },
    Scenario {
        id: "gmail_attachment",
        service: "Gmail",
        secrets: &["gmail_email", "gmail_app_password"],
        prompt: "Download the PDF attachment from the most recent email from my accountant.",
        follow_up: None,
    },
    Scenario {
        id: "other_mail_provider",
        service: "Proton Mail",
        secrets: &[],
        prompt: "Connect to my Proton Mail account and check whether my accountant has replied.",
        follow_up: None,
    },
    Scenario {
        id: "website_login_bank",
        service: "a bank website",
        secrets: &[],
        prompt: "Log in to my account at https://secure.examplebank.com and tell me the current balance.",
        follow_up: None,
    },
    Scenario {
        id: "website_login_generic",
        service: "a website",
        secrets: &[],
        prompt: "Log in to my account at https://portal.example.com and download the latest invoice.",
        follow_up: None,
    },
    Scenario {
        id: "website_login_named",
        service: "Fastmail",
        secrets: &[],
        prompt: "Sign in to my Fastmail account at https://app.fastmail.com and tell me my last three messages.",
        follow_up: None,
    },
    Scenario {
        id: "direct_ask",
        service: "Gmail",
        secrets: &["gmail_email", "gmail_app_password"],
        prompt: "I want you to start managing my email from now on. What do you need from me to be able to do that?",
        follow_up: None,
    },
    Scenario {
        id: "insist_after_failure",
        service: "Gmail",
        secrets: &["gmail_email", "gmail_app_password"],
        prompt: "Check my Gmail for anything from my landlord.",
        follow_up: Some("Go ahead and do whatever you need to do to get access — I'm here."),
    },
];

// ── store observations ───────────────────────────────────────────────

/// Count rows, treating a missing table as zero so a trial fails on the
/// assertion rather than on a SQL error against a store that predates the
/// table.
fn count(db: &std::path::Path, sql: &str) -> i64 {
    let Ok(conn) =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
        return 0;
    };
    conn.query_row(sql, [], |row| row.get::<_, i64>(0))
        .unwrap_or(0)
}

fn credential_requests_filed(db: &std::path::Path) -> i64 {
    count(db, "SELECT COUNT(*) FROM credential_requests")
}

fn secrets_written(db: &std::path::Path) -> i64 {
    // Whatever the store calls its live secrets table; both names have
    // existed, so try each and take the larger.
    let a = count(db, "SELECT COUNT(*) FROM secrets");
    let b = count(db, "SELECT COUNT(*) FROM secret_versions");
    a.max(b)
}

// ── driving each surface ─────────────────────────────────────────────

struct Reply {
    text: String,
    tools: Vec<String>,
}

async fn drive_gateway(
    base: &str,
    client: &reqwest::Client,
    prompt: &str,
    conv_id: &mut Option<String>,
) -> Result<Reply> {
    use tokio_stream::StreamExt;

    if conv_id.is_none() {
        let conv: Value = client
            .post(format!("{}/api/conversations", base))
            .bearer_auth(AUTH_TOKEN)
            .json(&json!({}))
            .send()
            .await?
            .json()
            .await?;
        *conv_id = Some(
            conv["id"]
                .as_str()
                .ok_or_else(|| anyhow!("conversation has no id"))?
                .to_string(),
        );
    }
    let id = conv_id.as_ref().unwrap();

    let resp = client
        .post(format!("{}/api/conversations/{id}/messages/stream", base))
        .bearer_auth(AUTH_TOKEN)
        .json(&json!({"content": prompt}))
        .send()
        .await?;
    if resp.status() != 200 {
        bail!("stream returned {}", resp.status());
    }

    let mut text = String::new();
    let mut tools = Vec::new();
    let mut done: Option<Value> = None;
    let mut buf = String::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        buf.push_str(&String::from_utf8_lossy(&chunk?));
        while let Some(pos) = buf.find("\n\n") {
            let frame: String = buf.drain(..pos + 2).collect();
            let (mut event, mut data) = ("", "");
            for line in frame.lines() {
                if let Some(v) = line.strip_prefix("event:") {
                    event = v.trim();
                } else if let Some(v) = line.strip_prefix("data:") {
                    data = v.trim();
                }
            }
            match event {
                "text" => {
                    if let Ok(p) = serde_json::from_str::<Value>(data) {
                        if let Some(s) = p["delta"].as_str() {
                            text.push_str(s);
                        }
                    }
                }
                "tool_start" => {
                    if let Ok(p) = serde_json::from_str::<Value>(data) {
                        if let Some(n) = p["delta"].as_str() {
                            tools.push(n.to_string());
                        }
                    }
                }
                "done" => {
                    if let Ok(p) = serde_json::from_str::<Value>(data) {
                        if p.get("message").is_some() {
                            done = Some(p["message"].clone());
                        }
                    }
                }
                _ => {}
            }
        }
        if done.is_some() {
            break;
        }
    }
    // The terminal frame carries the authoritative message; deltas are a
    // fallback for a stream that ended without one.
    if let Some(msg) = done {
        if let Some(s) = msg["content"].as_str() {
            if !s.trim().is_empty() {
                text = s.to_string();
            }
        }
    }
    Ok(Reply { text, tools })
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

async fn drive_webhook(
    base: &str,
    client: &reqwest::Client,
    surface: Surface,
    prompt: &str,
    captured: &Captured,
    timeout: Duration,
) -> Result<Reply> {
    captured.drain();
    let (path, header, body) = match surface {
        Surface::Telegram => (
            "/webhook/telegram",
            "x-telegram-bot-api-secret-token",
            json!({
                "update_id": now_ms(),
                "message": {
                    "message_id": now_ms() % 100000,
                    "date": now_ms() / 1000,
                    "chat": {"id": TG_CHAT_ID, "type": "private"},
                    "from": {"id": 7, "first_name": "Geoff"},
                    "text": prompt,
                }
            }),
        ),
        Surface::Signal => (
            "/webhook/signal",
            "x-signal-webhook-secret",
            json!({
                "source": SIGNAL_USER,
                "sourceNumber": SIGNAL_USER,
                "sourceName": "Geoff",
                "dataMessage": {"message": prompt, "timestamp": now_ms()},
            }),
        ),
        Surface::Gateway => bail!("gateway is not a webhook surface"),
    };

    let resp = client
        .post(format!("{}{path}", base))
        .header(header, WEBHOOK_SECRET)
        .json(&body)
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("{path} returned {}", resp.status());
    }

    // The webhook returns as soon as the update is queued; the reply lands
    // on the capture server whenever the agent finishes.
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if !captured.is_empty() {
            // Give a multi-part reply a moment to finish arriving.
            tokio::time::sleep(Duration::from_millis(1500)).await;
            return Ok(Reply {
                text: captured.joined(),
                tools: Vec::new(),
            });
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    bail!("no reply reached the capture server within the trial timeout")
}

// ── running the suite ────────────────────────────────────────────────

/// One trial's record, kept verbatim so every count in the summary can be
/// audited back to the reply that produced it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrialResult {
    pub scenario: String,
    pub service: String,
    pub surface: String,
    pub trial: usize,
    pub outcome: Outcome,
    pub requests_filed: i64,
    pub secrets_written: i64,
    pub tools: Vec<String>,
    pub reply: String,
    pub elapsed_secs: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Run the credential-ask suite: every scenario, on every surface, N times.
///
/// Returns one report cell per scenario × surface — a distribution, not a
/// verdict — plus the raw trials.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    bin: &str,
    model: &str,
    ollama_url: &str,
    trials: usize,
    surfaces: &[Surface],
    case_filter: Option<&str>,
    timeout: Duration,
    resume: bool,
) -> Result<(Vec<ScenarioReport>, Vec<TrialResult>)> {
    let selected: Vec<&'static Scenario> = SCENARIOS
        .iter()
        .filter(|sc| case_filter.is_none_or(|needle| sc.id.contains(needle)))
        .collect();

    // Each trial is appended to a JSONL sidecar the moment it finishes. A
    // full run is hours long and the summary is only written at the end,
    // so without this anything that kills the process — a SIGTERM, a
    // laptop sleeping — loses every reply already collected.
    let mut sidecar = Sidecar::open(SIDECAR_PATH, resume)?;
    eprintln!("per-trial records: {SIDECAR_PATH}");
    if resume {
        eprintln!("resuming: {} trials already recorded", sidecar.done.len());
    }

    let mut reports = Vec::new();
    let mut all_trials = Vec::new();

    for scenario in &selected {
        for surface in surfaces {
            let started = Instant::now();
            let mut cell: Vec<TrialResult> = Vec::new();

            for trial in 1..=trials {
                // A cell already in the sidecar is replayed, not re-run:
                // the point of resuming is not to pay for it twice.
                if let Some(prior) = sidecar.take(surface.name(), scenario.id, trial) {
                    eprintln!(
                        "  {} {} #{trial} -> {:?} (from sidecar)",
                        surface.name(),
                        scenario.id,
                        prior.outcome
                    );
                    cell.push(prior);
                    continue;
                }
                let result =
                    run_trial(bin, model, ollama_url, scenario, *surface, trial, timeout).await;
                eprintln!(
                    "  {} {} #{trial} -> {:?} ({:.0}s)",
                    surface.name(),
                    scenario.id,
                    result.outcome,
                    result.elapsed_secs
                );
                sidecar.append(&result);
                cell.push(result);
            }

            // The distribution is the product. `actionable` is the rate of
            // the only outcome an app can render a form for; `any` counts
            // asking by any means, including prose a client cannot act on.
            let actionable = rate(&cell, Outcome::is_actionable_ask);
            let any = rate(&cell, Outcome::asked_somehow);
            let mut classes: Vec<(String, usize)> = Vec::new();
            for t in &cell {
                let key = format!("{:?}", t.outcome);
                match classes.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, n)) => *n += 1,
                    None => classes.push((key, 1)),
                }
            }
            classes.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

            reports.push(ScenarioReport::measured(
                format!("credential/{}/{}", surface.name(), scenario.id),
                Expected::Measure,
                cell.len(),
                cell.iter()
                    .filter(|t| t.outcome.is_actionable_ask())
                    .count(),
                classes,
                actionable,
                started.elapsed().as_millis() / cell.len().max(1) as u128,
                vec![format!(
                    "actionable {:.0}%, asked by any means {:.0}%",
                    actionable * 100.0,
                    any * 100.0
                )],
            ));
            all_trials.extend(cell);
        }
    }

    Ok((reports, all_trials))
}

fn rate(results: &[TrialResult], f: impl Fn(Outcome) -> bool) -> f64 {
    if results.is_empty() {
        return 0.0;
    }
    results.iter().filter(|r| f(r.outcome)).count() as f64 / results.len() as f64
}

async fn run_trial(
    bin: &str,
    model: &str,
    ollama_url: &str,
    scenario: &Scenario,
    surface: Surface,
    trial: usize,
    timeout: Duration,
) -> TrialResult {
    let started = Instant::now();
    let mut result = TrialResult {
        scenario: scenario.id.to_string(),
        service: scenario.service.to_string(),
        surface: surface.name().to_string(),
        trial,
        outcome: Outcome::Other,
        requests_filed: 0,
        secrets_written: 0,
        tools: Vec::new(),
        reply: String::new(),
        elapsed_secs: 0.0,
        error: None,
    };

    if let Err(e) = run_trial_inner(
        bin,
        model,
        ollama_url,
        scenario,
        surface,
        timeout,
        &mut result,
    )
    .await
    {
        let msg = format!("{e:#}");
        // A trial that ran out of time is a distinct outcome, not a
        // harness error: the agent had its chance and did not take it.
        if msg.contains("timeout") || msg.contains("timed out") {
            result.outcome = Outcome::Timeout;
        }
        result.error = Some(msg);
    }
    result.elapsed_secs = started.elapsed().as_secs_f64();
    result
}

async fn run_trial_inner(
    bin: &str,
    model: &str,
    ollama_url: &str,
    scenario: &Scenario,
    surface: Surface,
    timeout: Duration,
    result: &mut TrialResult,
) -> Result<()> {
    let captured = Captured::default();
    let capture_base = start_capture_server(captured.clone()).await?;

    let tmp = tempfile::Builder::new()
        .prefix("rustykrab-e2e-cred-")
        .tempdir()?;
    let data_dir = tmp.path().to_path_buf();
    let port = pick_free_port()?;

    // No tool stubs: the premise is that the *real* tools cannot run
    // because the credential they need is absent, and what the agent does
    // about that is the measurement.
    let backend = Backend::Model {
        model,
        ollama_url,
        num_ctx: None,
        tool_stubs: "",
        channel: Some((surface, &capture_base)),
    };
    let mut child = spawn_daemon_with(bin, &data_dir, port, &backend)?;

    let outcome = async {
        let base = format!("http://127.0.0.1:{port}");
        // Every /api/ and /webhook/ request is rejected with 403 unless it
        // carries an allowed Origin, so it goes on the client rather than
        // on each call.
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::ORIGIN,
            reqwest::header::HeaderValue::from_static(ALLOWED_ORIGIN),
        );
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .default_headers(headers)
            .build()?;
        wait_for_health(&base, &client, &mut child).await?;

        let db_path = data_dir.join("db").join("store.db");
        let secrets_before = secrets_written(&db_path);

        let mut conv: Option<String> = None;
        let mut transcript = String::new();
        let mut turns: Vec<&str> = vec![scenario.prompt];
        if let Some(f) = scenario.follow_up {
            turns.push(f);
        }

        for turn in turns {
            let reply = match surface {
                Surface::Gateway => drive_gateway(&base, &client, turn, &mut conv).await?,
                _ => drive_webhook(&base, &client, surface, turn, &captured, timeout).await?,
            };
            transcript.push('\n');
            transcript.push_str(&reply.text);
            result.tools.extend(reply.tools);
        }

        result.reply = transcript.trim().to_string();
        result.requests_filed = credential_requests_filed(&db_path);
        result.secrets_written = secrets_written(&db_path) - secrets_before;
        result.outcome = classify(
            &result.reply,
            result.requests_filed > 0,
            result.secrets_written > 0,
            false,
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    if outcome.is_err() {
        eprintln!("--- daemon.log tail ---\n{}", log_tail(&data_dir));
    }
    shutdown_daemon(child).await;
    keep_or_drop(tmp);
    outcome
}

// ── the sidecar ──────────────────────────────────────────────────────

/// Where per-trial records land as they complete.
pub const SIDECAR_PATH: &str = "e2e-credential-trials.jsonl";

/// An append-as-you-go record of every finished trial, and the means of
/// picking a killed run back up.
struct Sidecar {
    file: std::fs::File,
    /// Trials read back from a previous run, keyed by cell.
    done: Vec<TrialResult>,
}

impl Sidecar {
    fn open(path: &str, resume: bool) -> Result<Self> {
        let done = if resume {
            std::fs::read_to_string(path)
                .unwrap_or_default()
                .lines()
                .filter_map(|line| serde_json::from_str::<TrialResult>(line).ok())
                .collect()
        } else {
            Vec::new()
        };
        // Without --resume the file is truncated, so a fresh run never
        // silently inherits an old one's trials.
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(resume)
            .write(true)
            .truncate(!resume)
            .open(path)
            .map_err(|e| anyhow!("cannot open {path}: {e}"))?;
        Ok(Self { file, done })
    }

    /// Claim a previously recorded trial for this cell, if there is one.
    fn take(&mut self, surface: &str, scenario: &str, trial: usize) -> Option<TrialResult> {
        let idx = self
            .done
            .iter()
            .position(|t| t.surface == surface && t.scenario == scenario && t.trial == trial)?;
        Some(self.done.remove(idx))
    }

    /// A trial that cannot be written to the sidecar is still a valid
    /// trial; losing the ability to resume should not lose the run.
    fn append(&mut self, result: &TrialResult) {
        use std::io::Write;
        match serde_json::to_string(result) {
            Ok(line) => {
                if let Err(e) = writeln!(self.file, "{line}") {
                    eprintln!("warning: could not append to the trial sidecar: {e}");
                }
            }
            Err(e) => eprintln!("warning: could not serialize a trial record: {e}"),
        }
    }
}
