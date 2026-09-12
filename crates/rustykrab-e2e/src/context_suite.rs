//! Task continuity across store -> channel -> runner -> Ollama wire -> action.
//! All history is synthetic. Domain tools are replaced, never real browser/code.
//! `context` is a deterministic boundary test, NOT a model-quality score.
//! `context-model` forwards to a loopback Ollama and scores observed actions.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{StatusCode, Uri};
use axum::response::IntoResponse;
use rustykrab_core::Tool;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use crate::surface::{Surface, TG_CHAT_ID, WEBHOOK_SECRET};
use crate::{
    artifact_dir, pick_free_port, shutdown_daemon, spawn_daemon_with, wait_for_health,
    write_json_artifact, Args, Backend, ALLOWED_ORIGIN, AUTH_TOKEN,
};

pub const CASES: &[&str] = &[
    "broadway-retained",
    "broadway-retained-explicit",
    "broadway-noisy",
    "broadway-summary-only",
    "broadway-explicit-reminder",
    "broadway-date-correction",
    "explicit-clock-switch",
    "missing-history-control",
    "compaction-loss-control",
    "compaction-generation-limit",
    "tool-availability-contract",
    "provider-trim-control",
    "telegram-provider-failure",
    "telegram-midrun-followup",
];
const ORIGINAL: &str = "Find Broadway shows in NYC with seating available September 14-16, 2026.";
const FOLLOWUP: &str = "Make use of the browser to fetch the time and date info";
const SUMMARY: &str = "Objective: Broadway availability September 14-16, 2026. Seating is not verified. The previous browser connection timed out. The user must manually use the Broadway.com date selector to check availability.";
const PRIOR: &str = "I found Broadway listings including Hamilton and Wicked, but could not verify seating for September 14-16, 2026 because the date selector requires an interactive browser.";
const CORRECTION: &str = "Use the browser for Broadway show times and available seats on September 18-20, 2026 instead of September 14-16.";
const CLOCK: &str = "New task: ignore the Broadway request. Use the browser to find the current date and time in UTC.";
const DATE: &str = "2026-09-10T07:00:00Z";
const THREAD: i64 = 2533;

fn message(role: &str, kind: &str, data: Value) -> Value {
    json!({"id": uuid::Uuid::new_v4(), "role": role,
        "content": {"type": kind, "data": data}, "created_at": DATE})
}

fn text(role: &str, content: &str) -> Value {
    message(role, "text", json!(content))
}

fn followup(case: &str) -> &'static str {
    match case {
        "explicit-clock-switch" => CLOCK,
        "broadway-date-correction" | "compaction-loss-control" | "compaction-generation-limit" => CORRECTION,
        "broadway-explicit-reminder" | "broadway-retained-explicit" => "Continue the Broadway task: use the browser to fetch show times and seating availability for September 14-16, 2026.",
        _ => FOLLOWUP,
    }
}

fn noisy(case: &str) -> bool {
    matches!(
        case,
        "broadway-noisy"
            | "broadway-explicit-reminder"
            | "compaction-loss-control"
            | "compaction-generation-limit"
            | "provider-trim-control"
    )
}

fn fixture(case: &str) -> Vec<Value> {
    if case == "missing-history-control" {
        return vec![];
    }
    let mut messages = vec![];
    if case != "broadway-summary-only" {
        messages.push(text("user", ORIGINAL));
    }
    messages.push(text("assistant", SUMMARY));
    if noisy(case) {
        // Deliberately content-heavy tool history, not a copy of private incident data.
        // No clock hints: the negative control must not leak its expected answer.
        for i in 0..10 {
            let id = format!("historical-fetch-{i}");
            messages.push(message(
                "assistant",
                "tool_call",
                json!({"id": id,
                "name":"web_fetch", "arguments":{"url":"https://www.broadway.com/shows/"}}),
            ));
            let content = format!("[EXTERNAL CONTENT]\n{}", (0..150).map(|n|
                format!("Listing {i}-{n}: theatre information, navigation links, accessibility, venue directions; no live seat inventory.\n")
            ).collect::<String>());
            messages.push(message(
                "tool",
                "tool_result",
                json!({"call_id":id,
                "output":{"content":content}, "is_error":false}),
            ));
        }
    }
    messages.push(text("assistant", PRIOR));
    messages
}

fn stubs() -> Result<String> {
    // BrowserManager::new has an opt-in process sweep. Never let schema-only
    // construction inherit that side effect from the operator's environment.
    if std::env::var("RUSTYKRAB_BROWSER_SWEEP").as_deref() == Ok("1") {
        bail!("unset RUSTYKRAB_BROWSER_SWEEP before running the isolated context eval");
    }
    // with_config avoids reading the operator's browser.json; none of these
    // constructors execute a request or launch a browser. Only schemas escape.
    let specs = [
        rustykrab_tools::BrowserTool::with_config(Default::default()).schema(),
        rustykrab_tools::WebSearchTool::new().schema(),
        rustykrab_tools::WebFetchTool::new().schema(),
        rustykrab_tools::CodeExecutionTool::new().schema(),
    ];
    let browser_ready = browser_ready_fixture();
    Ok(json!({"mode":"replace", "keep":["tools_list","tools_load"], "tools": specs.into_iter().map(|schema| {
        let failure = json!({"type":"err","kind":"timeout",
            "message":"Controlled evaluation: service unavailable; no external action was performed."});
        let responses = if browser_ready && schema.name == "browser" {
            // Neutral prerequisite signal only: no site, dates, page content,
            // applied navigation or task answer is supplied by the fixture.
            vec![json!({"type":"ok","value":{"running":true,"tabs":[],
                "fixture":"Inert browser readiness signal only; no navigation or external action performed."}}), failure]
        } else { vec![failure] };
        json!({"name":schema.name,"description":schema.description,
            "parameters":schema.parameters,
            "script":{"responses":responses}})
    }).collect::<Vec<_>>()}).to_string())
}

fn browser_ready_fixture() -> bool {
    std::env::var("RUSTYKRAB_CONTEXT_BROWSER_READY").as_deref() == Ok("1")
}

#[derive(Clone)]
struct Observer {
    requests: Arc<Mutex<Vec<Value>>>,
    replies: Arc<Mutex<Vec<String>>>,
    upstream: Option<String>,
    client: reqwest::Client,
    stopped: watch::Receiver<bool>,
    case: String,
    resume: Arc<tokio::sync::Notify>,
}

/// Owns both listener and active upstream request cancellation, even on errors.
pub(crate) struct Capture {
    observer: Observer,
    base: String,
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl Capture {
    pub(crate) fn url(&self) -> &str {
        &self.base
    }
    pub(crate) fn records(&self) -> Vec<Value> {
        self.observer.requests.lock().unwrap().clone()
    }
}

/// Dropping a cancelled trial must not leave its isolated daemon running.
struct TrialProcess(Option<std::process::Child>);
impl Drop for TrialProcess {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        self.task.abort();
    }
}

fn fixture_response(body: &Value) -> Value {
    if body["stream"] == false {
        // Adversarial summarizer omits the latest instruction. The eval must
        // detect the loss, not let a cooperative fake hide the missing pin.
        return json!({"model":"context-fixture", "message":{"role":"assistant","content":SUMMARY},
            "done":true,"done_reason":"stop","prompt_eval_count":100,"eval_count":20});
    }
    let msgs = body["messages"].as_array().cloned().unwrap_or_default();
    let has_eval_result = msgs.iter().any(|m| {
        m["role"] == "tool"
            && m["content"]
                .as_str()
                .unwrap_or("")
                .contains("Controlled evaluation")
    });
    let browser_exposed = body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|t| t["function"]["name"] == "browser");
    let function = if has_eval_result {
        json!({"name":"task_complete","arguments":{"summary":"Broadway availability remains unverified: the browser service is unavailable."}})
    } else if !browser_exposed {
        json!({"name":"tools_load","arguments":{"names":["browser","web_search","web_fetch","code_execution"]}})
    } else {
        json!({"name":"browser","arguments":{"action":"navigate","url":"https://www.broadway.com/shows/find-by-date/"}})
    };
    json!({"model":"context-fixture", "message":{"role":"assistant","content":"","tool_calls":[{"function":function}]},
        "done":true,"done_reason":"stop","prompt_eval_count":100,"eval_count":20})
}

async fn handle(State(state): State<Observer>, uri: Uri, bytes: Bytes) -> impl IntoResponse {
    let (status, content_type, output) = handle_request(state, uri, bytes).await;
    (status, [("content-type", content_type)], output)
}

async fn handle_request(
    state: Observer,
    uri: Uri,
    bytes: Bytes,
) -> (StatusCode, &'static str, String) {
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(json!({}));
    let path = uri.path();
    if path.contains("getUpdates") {
        tokio::time::sleep(Duration::from_millis(100)).await;
        return (
            StatusCode::OK,
            "application/json",
            json!({"ok":true,"result":[]}).to_string(),
        );
    }
    if path.starts_with("/bot") {
        if path.ends_with("sendMessage") {
            if let Some(s) = body["text"].as_str() {
                state.replies.lock().unwrap().push(s.to_owned());
            }
        }
        return (StatusCode::OK, "application/json", json!({"ok":true,"result":{"message_id":1,"date":1,"chat":{"id":TG_CHAT_ID,"type":"private"}}}).to_string());
    }
    if !matches!(path, "/api/chat" | "/api/show") {
        return (StatusCode::NOT_FOUND, "application/json", "{}".into());
    }
    let index = if path == "/api/chat" {
        let mut records = state.requests.lock().unwrap();
        let index = records.len();
        records.push(json!({"sequence":index,"wire_request":body,"wire_bytes":bytes.len()}));
        records[index]["wire_body_utf8"] = json!(String::from_utf8_lossy(&bytes));
        records[index]["wire_sha256"] = json!(format!("{:x}", Sha256::digest(&bytes)));
        Some(index)
    } else {
        None
    };
    let started = Instant::now();
    let result: Result<(u16, String)> = if let Some(upstream) = &state.upstream {
        let mut stopped = state.stopped.clone();
        tokio::select! {
            _ = stopped.changed() => Err(anyhow::anyhow!("trial cancelled")),
            response = async {
                let response = state.client.post(format!("{upstream}{path}"))
                    .header("Content-Type", "application/json").body(bytes).send().await?;
                let status = response.status().as_u16();
                Ok((status, response.text().await?))
            } => response,
        }
    } else if path == "/api/show" {
        Ok((200,json!({"capabilities":["completion","tools"],"model_info":{"gemma.context_length":65536}}).to_string()))
    } else if state.case == "tool-availability-contract" && index == Some(0) {
        Ok((
            200,
            format!(
                "{}\n",
                json!({"message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"tools_load","arguments":{"names":["browser","memory_search"]}}}]},"done":true,"done_reason":"stop","prompt_eval_count":100,"eval_count":20})
            ),
        ))
    } else if state.case == "telegram-midrun-followup" {
        if index == Some(0) {
            state.resume.notified().await;
        }
        Ok((
            200,
            format!(
                "{}\n",
                json!({"message":{"role":"assistant","content":"The Broadway dates remain unverified."},"done":true,"done_reason":"stop","prompt_eval_count":100,"eval_count":20})
            ),
        ))
    } else if state.case == "telegram-provider-failure" && index.is_some_and(|i| i > 0) {
        Ok((
            400,
            json!({"error":"controlled provider failure after browser timeout"}).to_string(),
        ))
    } else if state.case == "compaction-generation-limit" && body["stream"] == false {
        let mut response = fixture_response(&body);
        response["done_reason"] = json!("length");
        response["message"]["content"] = json!("INTENT: Broadway\nCONSTRAINTS: return date");
        Ok((200, format!("{response}\n")))
    } else {
        Ok((200, format!("{}\n", fixture_response(&body))))
    };
    let (status, output) =
        result.unwrap_or_else(|e| (502, json!({"error":e.to_string()}).to_string()));
    if let Some(index) = index {
        let mut records = state.requests.lock().unwrap();
        records[index]["http_status"] = json!(status);
        records[index]["elapsed_ms"] = json!(started.elapsed().as_millis());
        // Do not preserve hidden reasoning text. Keep observable output, tools,
        // and terminal metadata separately from the unmodified request body.
        records[index]["response"] = response_observation(&output);
    }
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
        if body["stream"] == true {
            "application/x-ndjson"
        } else {
            "application/json"
        },
        output,
    )
}

fn response_observation(output: &str) -> Value {
    let mut text = String::new();
    let mut calls = vec![];
    let mut terminal = Value::Null;
    let mut reasoning_bytes = 0;
    let mut malformed = 0;
    for line in output.lines().filter(|l| !l.trim().is_empty()) {
        match serde_json::from_str::<Value>(line) {
            Ok(v) => {
                text.push_str(v["message"]["content"].as_str().unwrap_or(""));
                reasoning_bytes += v["message"]["thinking"].as_str().unwrap_or("").len();
                if let Some(ts) = v["message"]["tool_calls"].as_array() {
                    calls.extend(ts.iter().cloned());
                }
                if v["done"] == true {
                    terminal = json!({"done":true,"done_reason":v["done_reason"],"prompt_eval_count":v["prompt_eval_count"],"eval_count":v["eval_count"]});
                }
            }
            Err(_) => malformed += 1,
        }
    }
    json!({"text":text,"tool_calls":calls,"terminal":terminal,"reasoning_bytes":reasoning_bytes,"malformed_lines":malformed})
}

pub(crate) async fn capture(
    upstream: Option<String>,
    timeout: Duration,
    case: &str,
) -> Result<Capture> {
    let (stop, stopped) = watch::channel(false);
    let observer = Observer {
        requests: Default::default(),
        replies: Default::default(),
        upstream,
        client: reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .build()?,
        stopped,
        case: case.to_owned(),
        resume: Arc::new(tokio::sync::Notify::new()),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let app = axum::Router::new()
        .fallback(axum::routing::any(handle))
        .layer(DefaultBodyLimit::max(4 * 1024 * 1024))
        .with_state(observer.clone());
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(Capture {
        observer,
        base,
        stop,
        task,
    })
}

/// Refuse an accidental cloud endpoint: these fixtures are local-model evals.
pub(crate) fn local_url(s: &str) -> Result<String> {
    let url = reqwest::Url::parse(s)?;
    if url.scheme() != "http"
        || !matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        bail!("context-model requires a plain loopback Ollama base URL without credentials, path, or query");
    }
    Ok(s.trim_end_matches('/').to_owned())
}

/// Join a real tools_load result to the next actor request's offered schemas.
/// Only adjacent actor exchanges are compared: historical activation reports
/// need not describe availability after later registry or capability changes.
fn tool_availability_check(records: &[Value]) -> Value {
    let actor: Vec<_> = records
        .iter()
        .filter(|r| r["wire_request"]["stream"] == true && r["wire_request"]["tools"].is_array())
        .collect();
    let mut checks = vec![];
    for pair in actor.windows(2) {
        if !pair[0]["response"]["tool_calls"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|c| c["function"]["name"] == "tools_load")
        {
            continue;
        }
        let report = pair[1]["wire_request"]["messages"]
            .as_array()
            .and_then(|messages| {
                let last_load = messages.iter().rposition(|m| {
                    m["role"] == "assistant"
                        && m["tool_calls"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .any(|c| c["function"]["name"] == "tools_load")
                })?;
                messages[last_load + 1..]
                    .iter()
                    .take_while(|m| m["role"] == "tool")
                    .filter_map(|m| serde_json::from_str::<Value>(m["content"].as_str()?).ok())
                    .find(|v| v["active"].is_array() && v["loaded"].is_array())
            });
        let offered: Vec<_> = pair[1]["wire_request"]["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| s["function"]["name"].as_str())
            .collect();
        let mut missing = vec![];
        if let Some(report) = &report {
            for field in ["active", "loaded"] {
                for name in report[field].as_array().into_iter().flatten() {
                    if name.as_str().is_none_or(|n| !offered.contains(&n)) {
                        missing.push(json!({"field":field,"name":name}));
                    }
                }
            }
        }
        checks.push(json!({"sequence":pair[1]["sequence"],"load_report":report,
            "offered":offered,"not_offered":missing,"passed":report.is_some() && missing.is_empty()}));
    }
    json!({"passed":checks.iter().all(|c| c["passed"] == true),"observed_load_transitions":checks.len(),"checks":checks})
}

fn wire_check(case: &str, records: &[Value]) -> Value {
    let availability = tool_availability_check(records);
    let ordinary: Vec<_> = records
        .iter()
        .filter(|r| r["wire_request"]["stream"] == true)
        .collect();
    let turns: Vec<_> = ordinary.iter().map(|r| {
        let messages = r["wire_request"]["messages"].as_array().cloned().unwrap_or_default();
        let contains = |needle: &str| messages.iter().any(|m| m["content"].as_str().unwrap_or("").contains(needle));
        let latest = messages.iter().any(|m| m["role"] == "user" && m["content"] == followup(case));
        let tool_names: Vec<_> = r["wire_request"]["tools"].as_array().into_iter().flatten()
            .filter_map(|t|t["function"]["name"].as_str()).collect();
        let can_discover = tool_names.contains(&"tools_list") && tool_names.contains(&"tools_load");
        let final_summary = messages.last().is_some_and(|m|m["role"] == "system" &&
            m["content"].as_str().unwrap_or("").starts_with("You have reached the iteration limit ("));
        json!({"sequence":r["sequence"],"messages":messages.len(),"original_exact":contains(ORIGINAL),
            "objective_present":contains("Broadway") && (contains("September 14-16") || contains("September 18-20")),
            "latest_user_exact":latest,"prior_answer_exact":contains(PRIOR),
            "system_first":messages.first().is_some_and(|m|m["role"] == "system"),
            "request_purpose":if final_summary {"iteration_cap_summary"} else {"agent_step"},
            "browser_available_or_discoverable":tool_names.contains(&"browser") || can_discover,
            "configured_tool_seed_honored":(["browser","web_search","web_fetch","code_execution"].iter().all(|t|tool_names.contains(t))),
            "tools":r["wire_request"]["tools"].as_array().map(|t|t.iter().map(|s|s["function"]["name"].clone()).collect::<Vec<_>>()),
            "options":r["wire_request"]["options"]})
    }).collect();
    let first = turns.first().cloned().unwrap_or(Value::Null);
    let missing = case == "missing-history-control";
    let compaction_loss = case == "compaction-loss-control";
    let trim = case == "provider-trim-control";
    let latest_all = !turns.is_empty() && turns.iter().all(|t| t["latest_user_exact"] == true);
    let objective = first["objective_present"] == true;
    let objective_all = !turns.is_empty() && turns.iter().all(|t| t["objective_present"] == true);
    let compactions = records
        .iter()
        .filter(|r| r["wire_request"]["stream"] == false)
        .count();
    // Negative controls pass ONLY when their intended perturbation was observed.
    // They do not claim that losing the newest user instruction is acceptable.
    let task_tools_available = !turns.is_empty()
        && turns
            .iter()
            .filter(|t| t["request_purpose"] == "agent_step")
            .all(|t| t["browser_available_or_discoverable"] == true);
    let configured_seed_honored = !turns.is_empty()
        && turns
            .iter()
            .filter(|t| t["request_purpose"] == "agent_step")
            .all(|t| t["configured_tool_seed_honored"] == true);
    let passed = !turns.is_empty()
        && availability["passed"] == true
        && (case != "tool-availability-contract"
            || availability["observed_load_transitions"]
                .as_u64()
                .unwrap_or(0)
                > 0)
        && configured_seed_honored
        && first["system_first"] == true
        && task_tools_available
        && if compaction_loss {
            compactions > 0 && latest_all && objective_all
        } else if trim {
            first["original_exact"] == false && latest_all
        } else if missing {
            !objective && latest_all
        } else {
            turns
                .iter()
                .all(|t| t["objective_present"] == true && t["prior_answer_exact"] == true)
                && latest_all
                && (case == "broadway-summary-only" || first["original_exact"] == true)
        };
    json!({"passed":passed,"turns":turns,"compaction_calls":compactions,
        "tool_availability_contract":availability,
        "negative_control":missing || trim,
        "continuity_invariant_passed":objective_all && latest_all,
        "task_tools_available":task_tools_available,
        "configured_seed_honored":configured_seed_honored,
        "known_defect_observed":compaction_loss && !latest_all})
}

/// Conservative deterministic rubric. Mentions in reasoning/history do not count.
/// A bare Google homepage is inconclusive, never evidence of Broadway progress.
fn behavior(case: &str, records: &[Value]) -> Value {
    let mut broadway = false;
    let mut clock = false;
    let mut correction = false;
    let mut actions = vec![];
    let mut invalid_calls = vec![];
    for record in records
        .iter()
        .filter(|r| r["wire_request"]["stream"] == true)
    {
        for call in record["response"]["tool_calls"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let f = &call["function"];
            let name = f["name"].as_str().unwrap_or("");
            let schema = record["wire_request"]["tools"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|t| t["function"]["name"] == name);
            let schema_error = match schema {
                Some(s) => rustykrab_core::validate_tool_args(
                    &s["function"]["parameters"],
                    &f["arguments"],
                )
                .err()
                .map(|e| e.to_string()),
                None => Some("tool was not offered in this request".to_owned()),
            };
            if let Some(error) = &schema_error {
                invalid_calls
                    .push(json!({"sequence":record["sequence"],"name":name,"error":error}));
            }
            if name == "task_complete" {
                continue;
            }
            let args = f["arguments"]
                .to_string()
                .to_lowercase()
                .replace('+', " ")
                .replace("%20", " ");
            let domain = matches!(name, "browser" | "web_search" | "web_fetch");
            broadway |= domain
                && (args.contains("broadway")
                    || args.contains("hamilton")
                    || args.contains("wicked"));
            clock |= (domain
                && (args.contains("worldtimeapi")
                    || args.contains("time.is")
                    || args.contains("timeanddate.com/worldclock")
                    || args.contains("current time")
                    || args.contains("current date")))
                || (name == "code_execution"
                    && (args.contains("datetime.now")
                        || args.contains("datetime.utcnow")
                        || args.contains("time.time(")));
            correction |= domain
                && (args.contains("18-20")
                    || args.contains("2026-09-18")
                    || args.contains("september 18"));
            actions.push(json!({"name":name,"arguments":f["arguments"],"offered_schema_valid":schema_error.is_none()}));
        }
    }
    let unscored = matches!(
        case,
        "missing-history-control" | "compaction-loss-control" | "provider-trim-control"
    );
    let on_task = if case == "explicit-clock-switch" {
        clock && !broadway
    } else {
        broadway && !clock && (case != "broadway-date-correction" || correction)
    };
    let schema_ok = invalid_calls.is_empty();
    json!({"passed":if unscored { Value::Null } else {json!(on_task && schema_ok)},
        "task_alignment_passed":if unscored {Value::Null} else {json!(on_task)},
        "tool_schema_contract_passed":schema_ok,"invalid_tool_calls":invalid_calls,
        "classification":if unscored {"control_unscored"} else if on_task && !schema_ok {"on_task_but_invalid_tool_call"} else if on_task {"on_task_action"}
            else if clock && case != "explicit-clock-switch" {"clock_task_drift"} else {"no_verified_on_task_action"},
        "broadway_action":broadway,"clock_action":clock,"new_dates_used":correction,"actions":actions,
        "scope":"bounded next-action continuity, not successful availability retrieval or semantic grading of every possible action"})
}

fn read_messages(db: &Path, id: &str) -> Result<Vec<Value>> {
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt =
        conn.prepare("SELECT data FROM messages WHERE conversation_id=?1 ORDER BY idx")?;
    let rows = stmt.query_map([id], |r| r.get::<_, String>(0))?;
    rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect()
}

fn context_compaction_strategy() -> Result<rustykrab_agent::CompactionStrategy> {
    match std::env::var("RUSTYKRAB_CONTEXT_COMPACTION_STRATEGY") {
        Ok(value) => Ok(serde_json::from_value(json!(value))?),
        Err(std::env::VarError::NotPresent) => Ok(Default::default()),
        Err(error) => Err(error.into()),
    }
}

/// Order is unambiguous: one isolated trial has one turn and sequential model
/// calls; routing/distillation are disabled. Refuse to assert a join if any
/// trace is missing, rather than silently pairing different requests.
fn transformation_audit(traces: &[Value], records: &[Value]) -> Value {
    if traces.len() != records.len() || traces.is_empty() {
        return json!({"matched":false,"trace_count":traces.len(),"wire_count":records.len()});
    }
    let pairs: Vec<_> = traces.iter().zip(records).map(|(trace,record)| {
        let empty = vec![];
        let before = trace["messages"].as_array().unwrap_or(&empty);
        let after = record["wire_request"]["messages"].as_array().unwrap_or(&empty);
        let text_messages: Vec<_> = before.iter().filter(|m|m["content"]["type"] == "text")
            .map(|m|json!({"id":m["id"],"role":m["role"],"characters":m["content"]["data"].as_str().unwrap_or("").chars().count(),
                "survived_exactly":after.iter().any(|wire|wire["role"] == m["role"] && wire["content"] == m["content"]["data"])})).collect();
        let ledger: Vec<_> = after.iter().enumerate().map(|(i,m)|json!({"index":i,"role":m["role"],
            "characters":m["content"].as_str().unwrap_or("").chars().count(),
            "tool_calls":m["tool_calls"].as_array().map_or(0,Vec::len)})).collect();
        json!({"sequence":record["sequence"],"trace_id":trace["trace_id"],
            "streaming_match":trace["streaming"] == record["wire_request"]["stream"],
            "pre_provider_messages":before.len(),"wire_messages":after.len(),
            "text_message_survival":text_messages,"wire_message_ledger":ledger})
    }).collect();
    let matched = pairs.iter().all(|p| p["streaming_match"] == true);
    json!({"matched":matched,"pairs":pairs,"correlation":"sequential calls in a single isolated turn, with trace IDs retained"})
}

fn seed(db: &Path, id: &str, messages: &[Value], surface: Surface) -> Result<()> {
    let mut conn = rusqlite::Connection::open(db)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    let tx = conn.transaction()?;
    let count: usize = tx.query_row(
        "SELECT count(*) FROM messages WHERE conversation_id=?1",
        [id],
        |r| r.get(0),
    )?;
    if count != 0 {
        bail!("refusing to seed a nonempty fixture conversation");
    }
    for (i, m) in messages.iter().enumerate() {
        tx.execute(
            "INSERT INTO messages(conversation_id,idx,data) VALUES(?1,?2,?3)",
            rusqlite::params![id, i, m.to_string()],
        )?;
    }
    if surface == Surface::Telegram {
        let address = rustykrab_store::ChannelAddress::Telegram {
            chat_id: TG_CHAT_ID,
            thread_id: THREAD,
        };
        tx.execute("INSERT INTO channel_bindings(channel,external_key,conv_id,created_at) VALUES(?1,?2,?3,?4)",
            rusqlite::params![address.channel(),address.external_key(),id,DATE])?;
    }
    tx.commit()?;
    Ok(())
}

async fn drive(
    client: &reqwest::Client,
    base: &str,
    id: &str,
    surface: Surface,
    content: &str,
    cap: &Capture,
) -> Result<()> {
    if surface == Surface::Gateway {
        let response = client
            .post(format!("{base}/api/conversations/{id}/messages"))
            .json(&json!({"content":content}))
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            bail!("gateway HTTP {status}: {}", response.text().await?);
        }
    } else {
        client
            .post(format!("{base}/webhook/telegram"))
            .header("x-telegram-bot-api-secret-token", WEBHOOK_SECRET)
            .json(&json!({"update_id":1,"message":{"message_id":1,"date":1,
                "chat":{"id":TG_CHAT_ID,"type":"private"},"from":{"id":7,"first_name":"Fixture"},
                "message_thread_id":THREAD,"text":content}}))
            .send()
            .await?
            .error_for_status()?;
        while cap.observer.replies.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    Ok(())
}

async fn trial(
    bin: &str,
    args: &Args,
    case: &str,
    surface: Surface,
    root: &Path,
    repetition: usize,
) -> Result<Value> {
    let live = args.mode == "context-model";
    let cap = capture(
        if live {
            Some(local_url(&args.ollama_url)?)
        } else {
            None
        },
        args.trial_timeout,
        case,
    )
    .await?;
    let tmp = tempfile::Builder::new()
        .prefix("rustykrab-context-")
        .tempdir()?;
    let dir = tmp.path();
    // A low threshold exercises real compaction. The separate trim control
    // disables runner compaction: oversized input must now be refused before
    // dispatch, not silently trimmed into a different request.
    let threshold = if matches!(
        case,
        "compaction-loss-control" | "compaction-generation-limit"
    ) {
        0.01
    } else if case == "provider-trim-control" {
        0.0
    } else {
        0.85
    };
    let compaction_strategy = context_compaction_strategy()?.name();
    std::fs::write(dir.join("harness.toml"),format!("name=\"context-eval\"\nagent_name=\"RustyKrab\"\nmax_iterations=4\nsoft_iteration_warning=0\nmax_tool_retries=0\ncompaction_threshold_pct={threshold}\ncompaction_strategy=\"{compaction_strategy}\"\n"))?;
    let port = pick_free_port()?;
    let base = format!("http://127.0.0.1:{port}");
    let stubs = stubs()?;
    let extra_env: Vec<(String, String)> = [
        ("RUSTYKRAB_DISTILL", "off"),
        ("RUSTYKRAB_PROMPT_LOG", "1"),
        ("HTTP_PROXY", "http://127.0.0.1:9"),
        ("HTTPS_PROXY", "http://127.0.0.1:9"),
        ("NO_PROXY", "127.0.0.1,localhost"),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v.into()))
    .collect();
    let mut child = TrialProcess(Some(spawn_daemon_with(
        bin,
        dir,
        port,
        &Backend::Model {
            model: &args.model,
            ollama_url: &cap.base,
            num_ctx: Some(if case == "provider-trim-control" {
                4096
            } else {
                65536
            }),
            active_tools: &["browser", "web_fetch", "web_search", "code_execution"],
            tool_stubs: &stubs,
            channel: if surface == Surface::Telegram {
                Some((surface, cap.base.as_str()))
            } else {
                None
            },
            extra_env: &extra_env,
        },
    )?));
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(args.trial_timeout)
        .default_headers({
            let mut h = reqwest::header::HeaderMap::new();
            h.insert("authorization", format!("Bearer {AUTH_TOKEN}").parse()?);
            h.insert("origin", ALLOWED_ORIGIN.parse()?);
            h
        })
        .build()?;
    let db = dir.join("db/store.db");
    let started = Instant::now();
    let initial = fixture(case);
    let mut id = String::new();
    let result: Result<()> = async {
        wait_for_health(
            &base,
            &client,
            child.0.as_mut().context("missing trial daemon")?,
        )
        .await?;
        let created: Value = client
            .post(format!("{base}/api/conversations"))
            .json(&json!({"title":"Synthetic Broadway continuity eval"}))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        id = created["id"]
            .as_str()
            .context("create conversation returned no id")?
            .to_owned();
        seed(&db, &id, &initial, surface)?;
        if read_messages(&db, &id)? != initial {
            bail!("seed round-trip mismatch");
        }
        tokio::time::timeout(args.trial_timeout, async {
            if case == "telegram-midrun-followup" {
                let original = drive(&client, &base, &id, surface, followup(case), &cap);
                let inject = async {
                    while cap.observer.requests.lock().unwrap().is_empty() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    client.post(format!("{base}/webhook/telegram"))
                        .header("x-telegram-bot-api-secret-token", WEBHOOK_SECRET)
                        .json(&json!({"update_id":2,"message":{"message_id":2,"date":2,
                            "chat":{"id":TG_CHAT_ID,"type":"private"},"from":{"id":7,"first_name":"Fixture"},
                            "message_thread_id":THREAD,"text":CORRECTION}}))
                        .send().await?.error_for_status()?;
                    // Release only after the actual channel admission path
                    // confirms the injection, not after an arbitrary delay.
                    loop {
                        if std::fs::read_to_string(dir.join("daemon.log")).unwrap_or_default()
                            .contains("injected user message into running agent loop") { break; }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    cap.observer.resume.notify_one();
                    Ok::<_, anyhow::Error>(())
                };
                tokio::try_join!(original, inject)?;
            } else {
                drive(&client, &base, &id, surface, followup(case), &cap).await?;
            }
            Ok::<_, anyhow::Error>(())
        }).await??;
        Ok(())
    }
    .await;
    shutdown_daemon(child.0.take().context("missing trial daemon")?).await;
    // Keep bounded environmental failure signals without copying complete
    // logs (which can include channel tokens, even in a synthetic fixture).
    let daemon_log = std::fs::read_to_string(dir.join("daemon.log"))?;
    let runtime_observations = json!({
        "working_memory_write_failed":daemon_log.contains("failed to persist inbound turn to working memory"),
        "embedding_initialization_failed":daemon_log.contains("fastembed init:")});
    let records = cap.observer.requests.lock().unwrap().clone();
    let final_messages = if id.is_empty() {
        vec![]
    } else {
        read_messages(&db, &id)?
    };
    let mut traces = vec![];
    if dir.join("logs").exists() {
        for entry in std::fs::read_dir(dir.join("logs"))? {
            let path = entry?.path();
            if path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .starts_with("prompts.log")
            {
                for line in std::fs::read_to_string(path)?.lines() {
                    let row: Value = serde_json::from_str(line)?;
                    if row["kind"] == "prompt" {
                        traces.push(row);
                    }
                }
            }
        }
    }
    let budget_guard = case == "provider-trim-control";
    let summary_guard = case == "compaction-generation-limit";
    let summary_refused = daemon_log.contains("compaction summary hit its generation limit");
    let refusal_observed = daemon_log
        .contains("refusing oversized Ollama request without dropping history")
        || result
            .as_ref()
            .err()
            .is_some_and(|e| e.to_string().contains("model input requires approximately"))
        || cap
            .observer
            .replies
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.contains("model input requires approximately"));
    // Compare canonical core-message serialization, not fixture omission of
    // optional defaults (e.g. ToolResult.images or agent_version).
    let canonical = |value: &Value| -> Result<Value> {
        let message: rustykrab_core::types::Message = serde_json::from_value(value.clone())?;
        Ok(serde_json::to_value(message)?)
    };
    let canonical_initial: Vec<_> = initial.iter().map(canonical).collect::<Result<_>>()?;
    let canonical_final: Vec<_> = final_messages
        .iter()
        .map(canonical)
        .collect::<Result<_>>()?;
    let original_trail_retained = canonical_initial
        .iter()
        .all(|old| canonical_final.contains(old));
    // Inspect the daemon's SQLite file through a separate read-only connection.
    // A model reply alone cannot prove that accepted/injected input is durable.
    let admission = if surface == Surface::Telegram {
        let conn =
            rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let mut stmt = conn.prepare(
            "SELECT message_id,data,status FROM channel_inbound WHERE conversation_id=?1",
        )?;
        let rows = stmt
            .query_map([&id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let inbound: Vec<_> = canonical_final
            .iter()
            .filter(|m| m["role"] == "user" && !initial.iter().any(|old| old["id"] == m["id"]))
            .collect();
        let mut checks = Vec::new();
        for message in inbound {
            let matched = rows.iter().find(|(id, _, _)| message["id"] == *id);
            let retained_exact = match matched {
                Some((_, data, status)) => {
                    status == "retained" && canonical(&serde_json::from_str(data)?)? == *message
                }
                None => false,
            };
            checks.push(json!({"message_id":message["id"],"retained_exact":retained_exact}));
        }
        json!({"passed":!checks.is_empty() && checks.iter().all(|c|c["retained_exact"] == true),"checks":checks,
            "scope":"same admission UUID and message bytes retained after daemon completion; not exactly-once upstream delivery"})
    } else {
        json!({"passed":null,"scope":"channel journal does not cover gateway admission"})
    };
    let latest_retained = final_messages
        .iter()
        .any(|m| m["role"] == "user" && m["content"]["data"] == followup(case));
    let wire = if budget_guard {
        json!({"passed":records.is_empty() && refusal_observed && original_trail_retained && latest_retained,
            "pre_dispatch_refusal":refusal_observed,"wire_requests":records.len(),
            "original_trail_retained":original_trail_retained,"latest_user_retained":latest_retained,
            "negative_control":false,"compaction_calls":0})
    } else if summary_guard {
        json!({"passed":summary_refused && records.len() == 1
                && records[0]["wire_request"]["stream"] == false
                && records[0]["response"]["terminal"]["done_reason"] == "length"
                && original_trail_retained && latest_retained,
            "summary_generation_refused":summary_refused,"wire_requests":records.len(),
            "original_trail_retained":original_trail_retained,"latest_user_retained":latest_retained,
            "negative_control":false,"compaction_calls":records.len()})
    } else {
        wire_check(case, &records)
    };
    let transformations = transformation_audit(&traces, &records);
    let runtime_regression_passed = match case {
        "telegram-provider-failure" => {
            records.iter().any(|r| r["http_status"] == 400)
                && final_messages
                    .iter()
                    .any(|m| m["role"] == "user" && m["content"]["data"] == followup(case))
                && final_messages
                    .iter()
                    .any(|m| m["role"] == "tool" && m.to_string().contains("Controlled evaluation"))
                && cap
                    .observer
                    .replies
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|r| r.contains("saved the partial history"))
        }
        "telegram-midrun-followup" => {
            records.len() == 2
                && records[1]["wire_request"]["messages"]
                    .as_array()
                    .is_some_and(|ms| {
                        ms.iter()
                            .any(|m| m["role"] == "user" && m["content"] == CORRECTION)
                    })
                && final_messages
                    .iter()
                    .filter(|m| m["role"] == "user" && m["content"]["data"] == CORRECTION)
                    .count()
                    == 1
        }
        _ => true,
    };
    let model = if live {
        behavior(case, &records)
    } else {
        json!({"passed":null,"classification":"not_tested_scripted_provider"})
    };
    let passed = (result.is_ok()
        || (budget_guard && refusal_observed)
        || (summary_guard && summary_refused))
        && admission["passed"] != false
        && runtime_regression_passed
        && wire["passed"] == true
        && model["passed"] != false
        && (budget_guard || (!traces.is_empty() && transformations["matched"] == true))
        && (budget_guard
            || summary_guard
            || final_messages.iter().any(|m| {
                m["role"] == "assistant" && !initial.iter().any(|old| old["id"] == m["id"])
            }));
    let evidence = json!({"case":case,"surface":surface.name(),"repetition":repetition,"passed":passed,
        "mode":args.mode,"binary":bin,"model":args.model,"compaction_strategy":compaction_strategy,
        "browser_first_response_ready":browser_ready_fixture(),"elapsed_ms":started.elapsed().as_millis(),
        "error":result.err().map(|e|e.to_string()),"history_source":"synthetic reconstruction, NOT historical wire replay",
        "seeded_history":initial,"submitted_followup":followup(case),"pre_provider_prompt_traces":traces,
        "wire_exchanges":records,"final_persisted_messages":final_messages,
        "captured_channel_replies":cap.observer.replies.lock().unwrap().clone(),
        "context_contract":wire,"model_behavior":model,
        "transformation_audit":transformations,
        "runtime_observations":runtime_observations,
        "admission_journal":admission,
        "runtime_regression_passed":runtime_regression_passed,
        "substitutes":["inert domain tools","synthetic history","empty memory/skill stores","loopback Telegram Bot API","routing and distillation disabled"],
        "uncertainty":["Does not recover the original incident payload or full tool catalog",
            "Wire request observed; Ollama chat-template rendered token text is not captured",
            "Proxy buffers response; latency and stream timing are not production-equivalent",
            "Keyword action rubric is conservative; schema validation is not tool execution or complete JSON Schema validation",
            "Four-iteration cap differs from production; this evaluates bounded continuity, not task completion"]});
    write_json_artifact(
        root,
        &format!("{case}-{}-{repetition}.json", surface.name()),
        &evidence,
    )?;
    Ok(
        json!({"case":case,"surface":surface.name(),"repetition":repetition,"passed":passed,
        "context_contract":wire,"model_behavior":model,"error":evidence["error"],"elapsed_ms":evidence["elapsed_ms"]}),
    )
}

pub async fn run(bin: &str, args: &Args) -> Result<bool> {
    if args.surfaces.is_empty() || args.surfaces.contains(&Surface::Signal) {
        bail!("context eval supports gateway and telegram, not signal");
    }
    let live = args.mode == "context-model";
    let selected: Vec<_> = CASES
        .iter()
        .copied()
        .filter(|id| args.case_filter.as_ref().is_none_or(|f| id.contains(f)))
        .filter(|id| !args.quick || !noisy(id))
        .filter(|id| {
            !id.starts_with("telegram-") || (args.surfaces.contains(&Surface::Telegram) && !live)
        })
        .filter(|id| !live || *id != "tool-availability-contract")
        .filter(|id| {
            !live
                || args.case_filter.is_some()
                || !matches!(
                    *id,
                    "compaction-loss-control"
                        | "compaction-generation-limit"
                        | "provider-trim-control"
                )
        })
        .collect();
    if selected.is_empty() {
        bail!("no context cases match the selection");
    }
    if live
        && selected.iter().any(|id| {
            id.ends_with("loss-control")
                || *id == "compaction-generation-limit"
                || id.starts_with("provider-trim")
        })
    {
        bail!("compaction/trim fault controls use a scripted summarizer and are context-mode only; select a behavioral case or --quick");
    }
    let root = artifact_dir().join(format!("context-{}", uuid::Uuid::new_v4()));
    let version = CommandVersion::read(bin)?;
    let mut metadata = json!({"mode":args.mode,"binary_version":version,"history":"synthetic, no private production export",
        "compaction_strategy":context_compaction_strategy()?.name(),
        "browser_first_response_ready":browser_ready_fixture(),
        "binary_sha256":format!("{:x}",Sha256::digest(std::fs::read(bin)?)),
        "fixture_and_grader_sha256":format!("{:x}",Sha256::digest(include_str!("context_suite.rs"))),
        "evaluator_sha256":format!("{:x}",Sha256::digest(std::fs::read(std::env::current_exe()?)?)),
        "claim":"active task and latest instruction survive each request; bounded actions remain on task",
        "tool_contract_source":"production schema methods linked from evaluator checkout; execution is inert",
        "uncertainty_register":["historical request bytes unavailable","domain tools substituted; reduced catalog uses actual checkout schemas","rendered model template not observed","real model not exercised in context mode"]});
    if live {
        let url = local_url(&args.ollama_url)?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(15))
            .build()?;
        metadata["ollama_show"] = client
            .post(format!("{url}/api/show"))
            .json(&json!({"model":args.model}))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        metadata["ollama_tags"] = client
            .get(format!("{url}/api/tags"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        metadata["ollama_version"] = client
            .get(format!("{url}/api/version"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
    }
    write_json_artifact(&root, "manifest.json", &metadata)?;
    let mut trials = vec![];
    for case in selected {
        for &surface in &args.surfaces {
            if case.starts_with("telegram-") && surface != Surface::Telegram {
                continue;
            }
            for repetition in 0..if live { args.reps } else { 1 } {
                eprintln!(
                    "context eval: {case} / {} / {}",
                    surface.name(),
                    repetition + 1
                );
                let result = tokio::select! {
                    result = trial(bin,args,case,surface,&root,repetition) => result,
                    _ = tokio::signal::ctrl_c() => bail!("context eval interrupted; completed evidence retained at {}",root.display()),
                };
                let row = match result {
                    Ok(row) => row,
                    Err(e) => {
                        json!({"case":case,"surface":surface.name(),"repetition":repetition,"passed":false,"error":e.to_string()})
                    }
                };
                eprintln!(
                    "  pass={} behavior={}",
                    row["passed"], row["model_behavior"]["classification"]
                );
                trials.push(row);
                write_json_artifact(&root, "progress.json", &json!({"trials":trials}))?;
            }
        }
    }
    let ok = trials.iter().all(|t| t["passed"] == true);
    let report = json!({"ok":ok,"mode":args.mode,"evidence_directory":root,"trials":trials,
        "grading":"all selected contract/behavior trials must pass; negative controls report loss detection, not acceptable production behavior"});
    write_json_artifact(&root, "report.json", &report)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(ok)
}

struct CommandVersion;
impl CommandVersion {
    fn read(bin: &str) -> Result<String> {
        // --version initializes the CLI logger before returning. Isolate even
        // this probe rather than letting it open the operator's data directory.
        let dir = tempfile::tempdir()?;
        let output = std::process::Command::new(bin)
            .env_clear()
            .env("RUSTYKRAB_DATA_DIR", dir.path())
            .arg("--version")
            .output()?;
        if !output.status.success() {
            bail!("cannot read tested daemon version");
        }
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn availability_audit_detects_phantom_active_tools_and_missing_reports() {
        let first = json!({"wire_request":{"stream":true,"tools":[]},"response":{"tool_calls":[{"function":{"name":"tools_load"}}]}});
        let load = json!({"role":"assistant","tool_calls":[{"function":{"name":"tools_load"}}]});
        let mut next = json!({"wire_request":{"stream":true,"tools":[{"function":{"name":"browser"}}],"messages":[load,{"role":"tool","content":json!({"active":["browser","memory_search"],"loaded":["browser"]}).to_string()}]}});
        assert_eq!(
            tool_availability_check(&[first.clone(), next.clone()])["passed"],
            false
        );
        next["wire_request"]["messages"][1]["content"] =
            json!(json!({"active":["browser"],"loaded":["browser"]}).to_string());
        let valid = tool_availability_check(&[first.clone(), next.clone()]);
        assert_eq!(valid["passed"], true);
        assert_eq!(valid["observed_load_transitions"], 1);
        // An old report cannot stand in for the newest call's missing result.
        next["wire_request"]["messages"]
            .as_array_mut()
            .unwrap()
            .push(load);
        assert_eq!(tool_availability_check(&[first, next])["passed"], false);
    }
    fn observed_call(name: &str, args: Value) -> Value {
        let specs: Value = serde_json::from_str(&stubs().unwrap()).unwrap();
        let schema = specs["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == name)
            .cloned()
            .unwrap_or(json!({"name":name,"parameters":{"type":"object"}}));
        json!({"wire_request":{"stream":true,"tools":[{"function":schema}]},"response":{"tool_calls":[{"function":{"name":name,"arguments":args}}]}})
    }
    #[test]
    fn task_alignment_does_not_hide_invalid_browser_actions() {
        let invalid = observed_call(
            "browser",
            json!({"action":"google_search","text":"Broadway"}),
        );
        let grade = behavior("broadway-retained", &[invalid]);
        assert_eq!(grade["task_alignment_passed"], true);
        assert_eq!(grade["tool_schema_contract_passed"], false);
        assert_eq!(grade["passed"], false);
        let valid = observed_call(
            "browser",
            json!({"action":"navigate","url":"https://www.broadway.com"}),
        );
        assert_eq!(behavior("broadway-retained", &[valid])["passed"], true);
    }
    #[test]
    fn clock_drift_fails_even_after_a_broadway_call() {
        let rows = vec![
            observed_call("browser", json!({"url":"https://www.broadway.com"})),
            observed_call("code_execution", json!({"code":"datetime.now()"})),
        ];
        assert_eq!(
            behavior("broadway-retained", &rows)["classification"],
            "clock_task_drift"
        );
    }
    #[test]
    fn clock_drift_recognizes_encoded_queries_and_worldclock_urls() {
        for url in [
            "https://www.google.com/search?q=current+time+in+NYC",
            "https://www.google.com/search?q=current%20time%20in%20NYC",
            "https://www.timeanddate.com/worldclock/new_york.html",
        ] {
            let row = observed_call("browser", json!({"action":"navigate","url":url}));
            assert_eq!(
                behavior("broadway-retained", &[row])["classification"],
                "clock_task_drift"
            );
        }
    }
    #[test]
    fn google_homepage_and_claimed_completion_are_not_progress() {
        let rows = vec![
            observed_call("browser", json!({"url":"https://www.google.com"})),
            observed_call("task_complete", json!({"summary":"Broadway done"})),
        ];
        assert_eq!(behavior("broadway-retained", &rows)["passed"], false);
    }
    #[test]
    fn explicit_clock_task_is_a_positive_control() {
        let rows = vec![observed_call(
            "web_fetch",
            json!({"url":"https://worldtimeapi.org/api/timezone/Etc/UTC"}),
        )];
        assert_eq!(behavior("explicit-clock-switch", &rows)["passed"], true);
        assert_eq!(behavior("broadway-retained", &rows)["passed"], false);
    }
    #[test]
    fn new_dates_need_action_evidence() {
        let old = observed_call("web_search", json!({"query":"Broadway September 14-16"}));
        let new = observed_call("web_search", json!({"query":"Broadway September 18-20"}));
        assert_eq!(
            behavior("broadway-date-correction", &[old])["passed"],
            false
        );
        assert_eq!(behavior("broadway-date-correction", &[new])["passed"], true);
    }
    #[test]
    fn wire_check_requires_actual_user_message_not_tool_echo() {
        let record = json!({"wire_request":{"stream":true,"messages":[{"role":"system","content":"system"},
            {"role":"user","content":ORIGINAL},{"role":"assistant","content":PRIOR},{"role":"tool","content":FOLLOWUP}]}});
        assert_eq!(wire_check("broadway-retained", &[record])["passed"], false);
        assert_eq!(wire_check("broadway-retained", &[])["passed"], false);
    }
    #[test]
    fn continuity_invariant_covers_later_requests_too() {
        let first = json!({"wire_request":{"stream":true,"messages":[
            {"role":"system","content":"system"},{"role":"user","content":ORIGINAL},
            {"role":"assistant","content":PRIOR},{"role":"user","content":FOLLOWUP}],
            "tools":[{"function":{"name":"browser"}}]}});
        let mut later = first.clone();
        later["wire_request"]["messages"] = json!([
            {"role":"system","content":"system"},{"role":"user","content":FOLLOWUP}]);
        let check = wire_check("broadway-retained", &[first, later]);
        assert_eq!(check["continuity_invariant_passed"], false);
        assert_eq!(check["passed"], false);
    }
    #[test]
    fn discoverability_does_not_hide_a_lost_configured_tool_seed() {
        let mut record = json!({"wire_request":{"stream":true,"messages":[
            {"role":"system","content":"system"},{"role":"user","content":ORIGINAL},
            {"role":"assistant","content":PRIOR},{"role":"user","content":FOLLOWUP}],
            "tools":[{"function":{"name":"tools_list"}},{"function":{"name":"tools_load"}}]}});
        let missing = wire_check("broadway-retained", &[record.clone()]);
        assert_eq!(missing["task_tools_available"], true);
        assert_eq!(missing["passed"], false);
        record["wire_request"]["tools"] =
            json!(["browser", "web_search", "web_fetch", "code_execution"]
                .iter()
                .map(|name| json!({"function":{"name":name}}))
                .collect::<Vec<_>>());
        assert_eq!(wire_check("broadway-retained", &[record])["passed"], true);
    }
    #[test]
    fn network_boundary_rejects_remote_and_credential_urls() {
        assert!(local_url("http://127.0.0.1:11434").is_ok());
        for url in [
            "https://example.com",
            "http://user:pass@localhost:11434",
            "http://localhost:11434/proxy",
            "http://localhost:11434?key=x",
        ] {
            assert!(local_url(url).is_err());
        }
    }
    #[test]
    fn response_evidence_does_not_store_reasoning_text() {
        let output = json!({"message":{"thinking":"hidden synthetic reasoning","content":"visible"},"done":true,"eval_count":9}).to_string();
        let observation = response_observation(&output);
        assert!(!observation
            .to_string()
            .contains("hidden synthetic reasoning"));
        assert_eq!(observation["terminal"]["eval_count"], 9);
    }
}
