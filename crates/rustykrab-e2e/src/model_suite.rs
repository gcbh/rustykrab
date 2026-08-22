//! Scenarios that run a real model through the whole daemon.
//!
//! Everything here talks to `gemma4:26b` over Ollama with the tool
//! registry replaced by stubs the scenario controls, so the interesting
//! failures — an upstream that dies once, one that never works, a result
//! too big for the window — happen on demand rather than by luck.
//!
//! A scenario gets a fresh daemon per repetition. That costs a few seconds
//! of boot each time and buys the only thing that makes repetitions
//! meaningful: no scenario can pass on state another one left behind, and
//! a flaky result is the model being flaky rather than the store being
//! dirty.
//!
//! Results are reported as a pass rate over repetitions. With a sampled
//! 26B model, "passed once" and "passed every time" are both misleading.

use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{json, Value};

use crate::assertion::{s, Assertion};
use crate::judge::{Judge, JudgeSpec};
use crate::transcript::Transcript;
use crate::{
    hex_decode, pick_free_port, spawn_daemon_with, wait_for_health, Backend, Ctx, Expected,
    ScenarioReport, MASTER_KEY_HEX,
};

/// Ceiling for one repetition, including daemon boot and every turn. A
/// local 26B model compacting a conversation is slow, but not this slow.
const CASE_TIMEOUT: Duration = Duration::from_secs(1_200);

/// Context budget for the compaction scenarios.
///
/// The trigger fires at 85% of the budget after a 25% response reserve, so
/// this compacts at roughly 3.8k tokens — reachable in a handful of turns
/// instead of the hour of local inference a 128k window would need. The
/// code path is identical; only the threshold moves.
const TIGHT_CONTEXT_TOKENS: usize = 6_000;

pub struct ModelCase {
    pub id: &'static str,
    pub description: &'static str,
    /// Skipped by `--quick`.
    pub slow: bool,
    /// User messages, sent in order to one conversation.
    pub turns: Vec<String>,
    /// Contents of the daemon's `RUSTYKRAB_TOOL_STUBS` file.
    pub stubs: Value,
    /// Contents of the daemon's `harness.toml`, when the scenario needs a
    /// non-default agent config (the compaction budget, mostly).
    pub harness_toml: Option<String>,
    pub assertions: Vec<Assertion>,
    pub judge: Option<JudgeSpec>,
}

impl ModelCase {
    fn new(id: &'static str, description: &'static str) -> Self {
        Self {
            id,
            description,
            slow: false,
            turns: Vec::new(),
            // Replace the registry by default: thirty real tools give a
            // small model thirty ways to wander off, and the scenario stops
            // measuring what it meant to.
            stubs: json!({ "mode": "replace", "keep": [], "tools": [] }),
            harness_toml: None,
            assertions: Vec::new(),
            judge: None,
        }
    }

    fn ask(mut self, turn: impl Into<String>) -> Self {
        self.turns.push(turn.into());
        self
    }

    fn slow(mut self) -> Self {
        self.slow = true;
        self
    }

    fn with_tool(mut self, tool: Value) -> Self {
        self.stubs["tools"].as_array_mut().unwrap().push(tool);
        self
    }

    /// Keep these real tools alongside the stubs — the memory scenarios
    /// need the genuine `memory_*` tools, since those are what they test.
    fn keeping(mut self, names: &[&str]) -> Self {
        self.stubs["keep"] = json!(names);
        self
    }

    fn with_harness(mut self, toml: impl Into<String>) -> Self {
        self.harness_toml = Some(toml.into());
        self
    }

    fn expect(mut self, a: Assertion) -> Self {
        self.assertions.push(a);
        self
    }

    fn judged(mut self, j: JudgeSpec) -> Self {
        self.judge = Some(j);
        self
    }
}

/// A tight agent config: small context so compaction is reachable, few
/// iterations so a confused model cannot grind for ten minutes.
fn tight_harness() -> String {
    format!(
        "name = \"e2e\"\n\
         agent_name = \"RustyKrab\"\n\
         max_iterations = 12\n\
         soft_iteration_warning = 0\n\
         max_consecutive_errors = 3\n\
         max_tool_retries = 2\n\
         max_context_tokens = {TIGHT_CONTEXT_TOKENS}\n"
    )
}

/// The default config for non-compaction scenarios: a normal window, but a
/// low iteration cap so a wandering model fails fast instead of slowly.
fn bounded_harness() -> String {
    "name = \"e2e\"\n\
     agent_name = \"RustyKrab\"\n\
     max_iterations = 10\n\
     soft_iteration_warning = 0\n\
     max_consecutive_errors = 3\n\
     max_tool_retries = 2\n\
     max_context_tokens = 128000\n"
        .to_string()
}

fn tool(
    name: &str,
    description: &str,
    properties: Value,
    required: Value,
    responses: Value,
) -> Value {
    json!({
        "name": name,
        "description": description,
        "parameters": { "type": "object", "properties": properties, "required": required },
        "script": { "responses": responses },
    })
}

/// Filler text used to inflate turns toward the compaction trigger.
fn bulky_note(index: usize) -> String {
    let mut out =
        format!("Here is batch {index} of the audit log. Acknowledge it in one short sentence.\n");
    for i in 0..40 {
        out.push_str(&format!(
            "{i:04} 2026-08-19T04:{:02}:11Z worker=ingest-{} status=ok latency_ms={} \
             note=nothing unusual observed in this window\n",
            i % 60,
            i % 7,
            80 + (i % 40)
        ));
    }
    out
}

pub fn cases() -> Vec<ModelCase> {
    vec![
        // ── basic ────────────────────────────────────────────────────
        ModelCase::new(
            "model-answers-a-question",
            "A single question through the whole daemon returns a correct answer",
        )
        .with_harness(bounded_harness())
        .ask("What is the capital city of Iceland? Answer in one short sentence.")
        .expect(Assertion::NoRunError)
        .expect(Assertion::FinalContainsAny(s(&["reykjav"])))
        .expect(Assertion::IterationsAtMost(2))
        .judged(JudgeSpec::new(
            "The answer states that Reykjavik is the capital of Iceland, in roughly one \
             sentence. Extra trivia is fine; a wrong city, a refusal, or a wall of text is not.",
            0.7,
        )),
        ModelCase::new(
            "model-keeps-multi-turn-context",
            "A fact from the first turn is still available three turns later",
        )
        .with_harness(bounded_harness())
        .ask("My deployment is codenamed Vireo. Just acknowledge that briefly.")
        .ask("I also run a nightly backup at 02:30. Acknowledge briefly.")
        .ask("What is my deployment codenamed?")
        .expect(Assertion::NoRunError)
        .expect(Assertion::FinalContainsAny(s(&["vireo"]))),
        ModelCase::new(
            "model-admits-ignorance",
            "The agent declines to invent facts it cannot have",
        )
        .with_harness(bounded_harness())
        .ask(
            "What was the exact closing share price of the company Vantablack Logistics \
             on 14 March 2031?",
        )
        .expect(Assertion::NoRunError)
        .expect(Assertion::FinalNonEmpty)
        .judged(JudgeSpec::new(
            "The answer makes clear it cannot know this — the date is in the future, or the \
             company is unknown to it, or it has no market data. Any answer that states or \
             estimates a specific share price scores 0.",
            0.7,
        )),
        ModelCase::new(
            "model-honours-a-tight-output-format",
            "A strict output-format instruction is followed exactly",
        )
        .with_harness(bounded_harness())
        .ask(
            "Reply with exactly three words separated by single commas and nothing else: \
             the three primary additive colours, lowercase.",
        )
        .expect(Assertion::NoRunError)
        .expect(Assertion::FinalContainsAll(s(&["red", "green", "blue"])))
        .expect(Assertion::FinalMatches(
            r"(?i)^\s*red\s*,\s*green\s*,\s*blue\s*\.?\s*$".to_string(),
        )),
        // ── tools ────────────────────────────────────────────────────
        ModelCase::new(
            "model-calls-one-tool",
            "The agent picks the right tool, fills its schema, and reports the result",
        )
        .with_harness(bounded_harness())
        .with_tool(tool(
            "weather_lookup",
            "Get the current weather for a city. Returns temperature in Celsius and conditions.",
            json!({ "city": { "type": "string", "description": "Name of the city" } }),
            json!(["city"]),
            // Odd values so a matching answer cannot be a lucky guess.
            json!([{ "type": "ok",
                     "value": { "city": "Reykjavik", "temperature_c": 17,
                                "conditions": "sleet" } }]),
        ))
        .ask("What is the weather in Reykjavik right now? Use the tool, then tell me.")
        .expect(Assertion::NoRunError)
        .expect(Assertion::ToolCalled("weather_lookup".into()))
        .expect(Assertion::ToolArgContains {
            tool: "weather_lookup".into(),
            pointer: "/city".into(),
            needle: "reykjavik".into(),
        })
        .expect(Assertion::FinalContainsAll(s(&["17", "sleet"]))),
        ModelCase::new(
            "model-fills-a-multi-argument-schema",
            "Every required field of a three-argument schema is filled correctly",
        )
        .with_harness(bounded_harness())
        .with_tool(tool(
            "book_room",
            "Reserve a meeting room. All three arguments are required.",
            json!({
                "room": { "type": "string", "description": "Room name" },
                "date": { "type": "string", "description": "ISO-8601 date, e.g. 2026-09-01" },
                "start_time": { "type": "string", "description": "24-hour time, e.g. 14:00" },
            }),
            json!(["room", "date", "start_time"]),
            json!([{ "type": "ok", "value": { "booked": true, "confirmation": "KRB-8842" } }]),
        ))
        .ask(
            "Book the Kelvin room for 9 September 2026 starting at 15:30, then give me the \
             confirmation code.",
        )
        .expect(Assertion::NoRunError)
        .expect(Assertion::ToolArgContains {
            tool: "book_room".into(),
            pointer: "/room".into(),
            needle: "kelvin".into(),
        })
        .expect(Assertion::ToolArgContains {
            tool: "book_room".into(),
            pointer: "/date".into(),
            needle: "2026-09-09".into(),
        })
        .expect(Assertion::ToolArgContains {
            tool: "book_room".into(),
            pointer: "/start_time".into(),
            needle: "15:30".into(),
        })
        .expect(Assertion::FinalContainsAny(s(&["KRB-8842"]))),
        ModelCase::new(
            "model-leaves-irrelevant-tools-alone",
            "A present but irrelevant tool is not called",
        )
        .with_harness(bounded_harness())
        .with_tool(tool(
            "send_invoice",
            "Email an invoice to a customer. Only use when explicitly asked to bill someone.",
            json!({
                "customer": { "type": "string", "description": "Customer name" },
                "amount": { "type": "string", "description": "Amount in USD" },
            }),
            json!(["customer", "amount"]),
            json!([{ "type": "ok", "value": { "sent": true } }]),
        ))
        .ask("How many millilitres are in a litre? Just answer.")
        .expect(Assertion::NoRunError)
        .expect(Assertion::ToolNotCalled("send_invoice".into()))
        .expect(Assertion::FinalContainsNone(s(&["invoice"])))
        .expect(Assertion::FinalContainsAny(s(&[
            "1000",
            "1,000",
            "one thousand",
        ]))),
        ModelCase::new(
            "model-recovers-from-a-transient-tool-error",
            "A tool that fails once and then succeeds still produces a correct answer",
        )
        .with_harness(bounded_harness())
        .with_tool(tool(
            "weather_lookup",
            "Get the current weather for a city. Returns temperature in Celsius and conditions.",
            json!({ "city": { "type": "string", "description": "Name of the city" } }),
            json!(["city"]),
            json!([
                { "type": "err", "message": "upstream weather service timed out after 30s",
                  "kind": "transient" },
                { "type": "ok", "value": { "city": "Reykjavik", "temperature_c": 17,
                                           "conditions": "sleet" } }
            ]),
        ))
        .ask("What is the weather in Reykjavik? Use the tool. Retry once if it errors.")
        .expect(Assertion::NoRunError)
        .expect(Assertion::ToolCallCount {
            tool: "weather_lookup".into(),
            min: 2,
            max: 5,
        })
        .expect(Assertion::RecoveredFrom {
            tool: "weather_lookup".into(),
            then_says: s(&["17", "sleet"]),
        }),
        ModelCase::new(
            "model-reports-a-permanent-tool-failure",
            "A tool that always fails is reported honestly, without fabrication or looping",
        )
        .with_harness(bounded_harness())
        .with_tool(tool(
            "stock_quote",
            "Get the latest share price for a ticker symbol.",
            json!({ "ticker": { "type": "string", "description": "Ticker symbol" } }),
            json!(["ticker"]),
            json!([{ "type": "err", "message": "quote provider returned 503: service unavailable",
                     "kind": "transient" }]),
        ))
        .ask("What is the current share price of NVDA? Use the tool.")
        .expect(Assertion::NoRunError)
        .expect(Assertion::ToolCalled("stock_quote".into()))
        .expect(Assertion::RetriesAtMost {
            tool: "stock_quote".into(),
            max: 8,
        })
        .expect(Assertion::FinalNonEmpty)
        .judged(JudgeSpec::new(
            "The answer says the price could not be retrieved because the tool or provider \
             failed. It must NOT state, guess, estimate, or recall any specific share price \
             for NVDA — doing so scores 0, however well hedged.",
            0.7,
        )),
        ModelCase::new(
            "model-distinguishes-empty-from-failed",
            "An empty-but-successful result is reported as empty, not as an error",
        )
        .with_harness(bounded_harness())
        .with_tool(tool(
            "search_orders",
            "Search customer orders. Returns a list, which may be empty.",
            json!({ "query": { "type": "string", "description": "Search terms" } }),
            json!(["query"]),
            json!([{ "type": "ok", "value": { "results": [], "count": 0 } }]),
        ))
        .ask("Find my orders for a titanium kettle. Use the tool and tell me what you find.")
        .expect(Assertion::NoRunError)
        .expect(Assertion::ToolCalled("search_orders".into()))
        .expect(Assertion::RetriesAtMost {
            tool: "search_orders".into(),
            max: 3,
        })
        .judged(JudgeSpec::new(
            "The answer says no matching orders were found. It must not claim an error or \
             failure occurred, and must not invent any order.",
            0.7,
        )),
        ModelCase::new(
            "model-chains-dependent-tool-calls",
            "One tool's output becomes the next tool's input",
        )
        .with_harness(bounded_harness())
        .with_tool(tool(
            "find_order",
            "Look up an order id from a customer name.",
            json!({ "customer": { "type": "string", "description": "Customer full name" } }),
            json!(["customer"]),
            json!([{ "type": "ok", "value": { "order_id": "ORD-51993" } }]),
        ))
        .with_tool(tool(
            "order_status",
            "Get the delivery status of an order by its order id.",
            json!({ "order_id": { "type": "string", "description": "Order id" } }),
            json!(["order_id"]),
            json!([{ "type": "ok", "value": { "status": "held at customs", "eta_days": 6 } }]),
        ))
        .ask(
            "Find the order for Wren Okonkwo and tell me its delivery status. \
             You will need both tools.",
        )
        .expect(Assertion::NoRunError)
        .expect(Assertion::ToolCallOrder(s(&["find_order", "order_status"])))
        .expect(Assertion::ToolArgContains {
            tool: "order_status".into(),
            pointer: "/order_id".into(),
            needle: "ORD-51993".into(),
        })
        .expect(Assertion::FinalContainsAny(s(&["customs"]))),
        // ── compaction ───────────────────────────────────────────────
        ModelCase::new(
            "compaction-does-not-fire-early",
            "A short conversation under budget is never compacted",
        )
        .with_harness(bounded_harness())
        .ask("We are planning a small migration next week. Acknowledge briefly.")
        .ask("In one sentence, what did I say I was planning?")
        .expect(Assertion::NoRunError)
        .expect(Assertion::Compacted(false))
        .expect(Assertion::FinalContainsAny(s(&["migration"]))),
        ModelCase::new(
            "compaction-triggers-and-keeps-history",
            "An over-budget conversation compacts, and no message is destroyed by it",
        )
        .slow()
        .with_harness(tight_harness())
        .ask("We are auditing the ingest pipeline. Acknowledge in one sentence.")
        .ask(bulky_note(1))
        .ask(bulky_note(2))
        .ask(bulky_note(3))
        .ask(bulky_note(4))
        .ask(bulky_note(5))
        .ask("Summarise where we are in one sentence.")
        .expect(Assertion::NoRunError)
        .expect(Assertion::Compacted(true))
        .expect(Assertion::CompactionGenerationAtLeast(1))
        .expect(Assertion::FoldedAtLeast(4))
        // Seven user turns plus their replies: compaction must hide
        // history, never delete it.
        .expect(Assertion::HistoryRetainedAtLeast(12))
        .expect(Assertion::FinalNonEmpty),
        ModelCase::new(
            "compaction-preserves-an-early-fact",
            "A fact stated before the bookmark survives the fold and stays answerable",
        )
        .slow()
        .with_harness(tight_harness())
        .ask(
            "Before we start: the staging cluster is named borealis and the incident owner \
             is Priya Raman. Acknowledge in one sentence.",
        )
        .ask(bulky_note(1))
        .ask(bulky_note(2))
        .ask(bulky_note(3))
        .ask(bulky_note(4))
        .ask(bulky_note(5))
        .ask("What is the staging cluster called, and who owns incidents for this service?")
        .expect(Assertion::NoRunError)
        .expect(Assertion::Compacted(true))
        // The summary is what the model will see from here on, so the fact
        // has to be in there — answering correctly from a tail that happened
        // not to be folded yet would prove nothing.
        .expect(Assertion::SummaryContainsAny(s(&["borealis"])))
        .expect(Assertion::FinalContainsAny(s(&["borealis"])))
        .judged(JudgeSpec::new(
            "The answer names the staging cluster as `borealis` AND names Priya Raman as the \
             incident owner. Both are required; either one missing or wrong scores below 0.5.",
            0.7,
        )),
        ModelCase::new(
            "compaction-keeps-the-newest-turn-verbatim",
            "The most recent instruction survives compaction word-for-word",
        )
        .slow()
        .with_harness(tight_harness())
        .ask("We are auditing the ingest pipeline. Acknowledge in one sentence.")
        .ask(bulky_note(1))
        .ask(bulky_note(2))
        .ask(bulky_note(3))
        .ask(bulky_note(4))
        .ask(bulky_note(5))
        .ask(
            "Forget the audit for a moment. My postgres replica is called selkie-2. \
             Repeat its name back to me exactly.",
        )
        .expect(Assertion::NoRunError)
        .expect(Assertion::Compacted(true))
        .expect(Assertion::FinalContainsAny(s(&["selkie-2"]))),
        // ── memory ───────────────────────────────────────────────────
        // These keep the real memory tools: the whole point is whether
        // hybrid retrieval finds the fact, so stubbing it would test
        // nothing.
        ModelCase::new(
            "memory-saves-when-asked",
            "An explicit request to remember something reaches the memory store",
        )
        .with_harness(bounded_harness())
        .keeping(&["memory_save", "memory_search", "memory_get"])
        .ask(
            "Please remember this for later: my postgres replica is called selkie-2. \
             Save it to memory, then confirm you have.",
        )
        .expect(Assertion::NoRunError)
        .expect(Assertion::ToolCalled("memory_save".into())),
        ModelCase::new(
            "memory-round-trip",
            "A fact saved in one turn is retrievable through memory in a later turn",
        )
        .slow()
        .with_harness(bounded_harness())
        .keeping(&["memory_save", "memory_search", "memory_get"])
        .ask("Remember that my kettle is a Stagg EKG Pro. Save that to memory.")
        .ask(
            "Search your memory: what model is my kettle? If it is not in memory, say so — \
             do not guess.",
        )
        .expect(Assertion::NoRunError)
        .expect(Assertion::ToolCalled("memory_save".into()))
        .expect(Assertion::ToolCalled("memory_search".into()))
        // Asserts retrieval actually returned the fact, separately from
        // whether the model then used it well.
        .expect(Assertion::ToolOutputContainsAny {
            tool: "memory_search".into(),
            needles: s(&["stagg"]),
        })
        .expect(Assertion::FinalContainsAny(s(&["stagg"]))),
        ModelCase::new(
            "memory-does-not-fabricate-on-a-miss",
            "A query with no matching memory produces an honest miss, not an invention",
        )
        .with_harness(bounded_harness())
        .keeping(&["memory_save", "memory_search", "memory_get"])
        .ask(
            "Search your memory for my bicycle's serial number and tell me what it is. \
             If it is not there, say so plainly.",
        )
        .expect(Assertion::NoRunError)
        .expect(Assertion::ToolCalled("memory_search".into()))
        .expect(Assertion::FinalNonEmpty)
        .judged(JudgeSpec::new(
            "The answer says no bicycle serial number is stored in memory. Any answer that \
             supplies a serial number, or claims to have found one, scores 0.",
            0.7,
        )),
    ]
}

/// Run the model suite. Returns the reports and the judge that graded.
pub async fn run(
    bin: &str,
    model: &str,
    ollama_url: &str,
    reps: usize,
    case_filter: Option<&str>,
    quick: bool,
) -> Result<(Vec<ScenarioReport>, String)> {
    preflight(model, ollama_url).await?;

    let judge = Judge::new(model, ollama_url);
    eprintln!("model suite: {model}, judged by {}", judge.name());

    let selected: Vec<ModelCase> = cases()
        .into_iter()
        .filter(|c| case_filter.is_none_or(|needle| c.id.contains(needle)))
        .filter(|c| !(quick && c.slow))
        .collect();

    let mut reports = Vec::new();
    for case in &selected {
        let started = Instant::now();
        let mut passes = 0;
        let mut details: Vec<String> = Vec::new();

        for _ in 0..reps {
            let transcript =
                match tokio::time::timeout(CASE_TIMEOUT, run_once(bin, model, ollama_url, case))
                    .await
                {
                    Ok(Ok(t)) => t,
                    Ok(Err(e)) => Transcript::failed(format!("{e:#}"), 0),
                    Err(_) => Transcript::failed(
                        format!("timed out after {}s", CASE_TIMEOUT.as_secs()),
                        0,
                    ),
                };

            let failures: Vec<String> = case
                .assertions
                .iter()
                .filter_map(|a| {
                    a.check(&transcript)
                        .err()
                        .map(|why| format!("{}: {why}", a.label()))
                })
                .collect();

            // Only grade a run whose hard assertions already passed:
            // judging an answer that failed a substring check wastes a
            // judge call and clutters the report.
            let verdict = match (&case.judge, failures.is_empty()) {
                (Some(spec), true) => Some(
                    judge
                        .grade(&case.turns.join("\n"), spec, &transcript.final_text)
                        .await,
                ),
                _ => None,
            };

            let passed = failures.is_empty() && verdict.as_ref().is_none_or(|v| v.passed);
            if passed {
                passes += 1;
            }
            for f in failures {
                if !details.contains(&f) {
                    details.push(f);
                }
            }
            if let Some(v) = verdict.filter(|v| !v.passed) {
                let line = format!("judge {:.2}: {}", v.score, v.reason);
                if !details.contains(&line) {
                    details.push(line);
                }
            }
        }

        let r = ScenarioReport::new(
            case.id,
            "model",
            Expected::Pass,
            reps,
            passes,
            details,
            started.elapsed().as_millis() / reps as u128,
        );
        eprintln!("{}", r.line());
        reports.push(r);
    }

    Ok((reports, judge.name().to_string()))
}

/// One repetition: a fresh daemon, every turn, then the persisted
/// conversation read back.
async fn run_once(
    bin: &str,
    model: &str,
    ollama_url: &str,
    case: &ModelCase,
) -> Result<Transcript> {
    let tmp = tempfile::Builder::new()
        .prefix("rustykrab-e2e-model-")
        .tempdir()?;
    let data_dir = tmp.path().to_path_buf();
    if let Some(toml) = &case.harness_toml {
        std::fs::write(data_dir.join("harness.toml"), toml)?;
    }

    let port = pick_free_port()?;
    let stubs = serde_json::to_string_pretty(&case.stubs)?;
    let backend = Backend::Model {
        model,
        ollama_url,
        tool_stubs: &stubs,
        // These scenarios drive the gateway directly; the credential
        // suite is the one that varies the surface.
        channel: None,
    };
    let mut child = spawn_daemon_with(bin, &data_dir, port, &backend)?;

    let result = async {
        let base = format!("http://127.0.0.1:{port}");
        // The origin-check middleware requires an Origin header on every
        // /api request (loopback origins are always allowed).
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::ORIGIN, base.parse()?);
        let client = reqwest::Client::builder()
            .default_headers(headers)
            // A local 26B model answering a compacted conversation can take
            // minutes; the per-repetition timeout is the real bound.
            .timeout(Duration::from_secs(900))
            .build()?;
        wait_for_health(&base, &client, &mut child).await?;

        // Opened after the daemon is healthy — it owns the database and
        // its migrations; this is a read handle for assertions.
        let store = rustykrab_store::Store::open(data_dir.join("db"), hex_decode(MASTER_KEY_HEX)?)?;
        let ctx = Ctx {
            base,
            client,
            secrets: store.secrets(),
            db_path: data_dir.join("db").join("store.db"),
            bin: bin.to_string(),
            data_dir: data_dir.clone(),
        };

        let started = Instant::now();
        let conv_id = ctx.create_conversation().await?;
        for turn in &case.turns {
            ctx.send(&conv_id, turn).await?;
        }
        let conv = ctx.conversation(&conv_id).await?;
        let mut transcript = Transcript::parse(&conv);
        transcript.duration_ms = started.elapsed().as_millis();
        Ok::<_, anyhow::Error>(transcript)
    }
    .await;

    if result.is_err() {
        eprintln!("--- daemon.log tail ---\n{}", crate::log_tail(&data_dir));
    }
    crate::shutdown_daemon(child).await;
    crate::keep_or_drop(tmp);
    result
}

/// Fail in one line if the machine cannot run the suite, rather than once
/// per scenario twenty minutes in.
async fn preflight(model: &str, ollama_url: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;

    let tags: Value = client
        .get(format!("{ollama_url}/api/tags"))
        .send()
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "Ollama is not reachable at {ollama_url}: {e}\nStart it with `ollama serve`."
            )
        })?
        .json()
        .await?;

    let available: Vec<String> = tags["models"]
        .as_array()
        .map(|models| {
            models
                .iter()
                .filter_map(|m| m["name"].as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if !available.iter().any(|m| m == model) {
        anyhow::bail!(
            "model {model} is not pulled. Available: {}\nPull it with `ollama pull {model}`.",
            if available.is_empty() {
                "(none)".into()
            } else {
                available.join(", ")
            }
        );
    }

    // Ollama serves one request at a time per model, so a busy client — a
    // running RustyKrab daemon is the usual one — does not slow the suite
    // down, it stops it. Better to say so now than to time out every case.
    let started = Instant::now();
    let probe = client
        .post(format!("{ollama_url}/api/chat"))
        .timeout(Duration::from_secs(120))
        .json(&json!({
            "model": model,
            "messages": [{ "role": "user", "content": "Reply with the single word: ready" }],
            "stream": false,
            "options": { "num_predict": 4, "temperature": 0 },
        }))
        .send()
        .await;
    match probe {
        Ok(r) if r.status().is_success() => {
            eprintln!(
                "warm-up: {model} responded in {:.1}s",
                started.elapsed().as_secs_f64()
            );
            Ok(())
        }
        Ok(r) => anyhow::bail!("Ollama returned HTTP {} for a trivial request", r.status()),
        Err(e) if e.is_timeout() => anyhow::bail!(
            "Ollama accepted the connection but produced nothing in 120s.\n\n\
             It serves one request at a time per model, so this usually means another client \
             holds the slot — a running RustyKrab daemon is the common culprit. Check \
             `ollama ps` and `ps aux | grep rustykrab`, or point the suite elsewhere with \
             --ollama-url."
        ),
        Err(e) => anyhow::bail!("the warm-up request failed: {e}"),
    }
}
