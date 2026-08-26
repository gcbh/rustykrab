//! Context-window ablation: sweep `num_ctx` and measure what each window
//! costs and what it buys.
//!
//! Three instruments per window:
//!
//! - **Speed probes**, straight against Ollama: the cost of switching to
//!   the window (KV realloc), resident memory, generation throughput, and
//!   prompt-processing throughput at a fixed size (cross-window
//!   comparability) and at a size scaled to the window (what big prompts
//!   actually cost there).
//! - **The full model suite** at that window, for accuracy. Compaction
//!   scenarios whose standard seed cannot reach the trigger at a given
//!   window are annotated rather than counted as failures — a scenario
//!   that cannot fire is not a scenario that failed.
//! - **A compaction cost probe**: history seeded past that window's
//!   trigger, run twice — expansion off, expansion on
//!   (`RUSTYKRAB_COMPACTION_EXPAND_CTX`) — on otherwise-identical input.
//!   This is the direct measurement behind "should compaction expand the
//!   window".
//!
//! The whole mode is a measurement, not a gate: it always exits 0 and
//! writes its findings to `e2e-ablation.json` / `e2e-ablation.md`.

use std::time::Duration;

use anyhow::Result;
use serde::Serialize;
use serde_json::{json, Value};

use crate::assertion::{s, Assertion};
use crate::model_suite::{self, ModelCase};

/// Mirrors the provider's fixed reserves: default `num_predict` (4096),
/// the assumed tool block (2048), framing (512). Approximate on purpose —
/// the exact figure depends on the measured tool block, and this only
/// steers seeding and annotation, never scoring.
const APPROX_RESERVES: usize = 4_096 + 2_048 + 512;
/// The runner's compaction ceiling default.
const COMPACTION_CEILING: usize = 65_536;
const TRIGGER_RATIO: f64 = 0.85;
/// Roughly what one `bulky_note` costs in tokens.
const NOTE_TOKENS: usize = 1_300;
/// What the three firing-compaction scenarios seed, roughly, in tokens.
const STANDARD_SEED_TOKENS: usize = 2_900;
/// Expansion target for the A/B probe. Twice the ceiling: big enough that
/// every history the ceiling permits fits in one summarizer call.
const EXPAND_TARGET: u32 = 131_072;

fn approx_input_budget(w: u32) -> usize {
    let w = w as usize;
    if w > APPROX_RESERVES {
        w - APPROX_RESERVES
    } else {
        (w / 4).max(512)
    }
}

/// Where the runner's compaction trigger sits at this window.
fn approx_threshold(w: u32) -> usize {
    (approx_input_budget(w).min(COMPACTION_CEILING) as f64 * TRIGGER_RATIO) as usize
}

/// Can the standard compaction scenarios (fixed two-note seed) reach the
/// trigger at this window?
fn standard_seed_fires(w: u32) -> bool {
    approx_threshold(w) <= STANDARD_SEED_TOKENS
}

const FIRING_COMPACTION_SCENARIOS: &[&str] = &[
    "compaction-triggers-and-keeps-history",
    "compaction-preserves-an-early-fact",
    "compaction-keeps-the-newest-turn-verbatim",
];

#[derive(Debug, Clone, Serialize)]
struct SpeedProbe {
    window: u32,
    /// KV realloc / model load for this window, seconds.
    load_secs: f64,
    /// Resident size after load, bytes, from /api/ps.
    size_bytes: u64,
    size_vram_bytes: u64,
    /// The context length Ollama reports actually serving. Makes the row
    /// self-validating: a request for a window the server quietly declines
    /// would otherwise be reported as a measurement of that window.
    #[serde(skip_serializing_if = "Option::is_none")]
    served_ctx: Option<u64>,
    /// Generation throughput, tokens/second.
    gen_tps: f64,
    /// Prompt-processing throughput on a fixed ~8k-token prompt.
    prompt_tps_fixed: f64,
    /// Prompt-processing throughput on a prompt scaled to ~60% of the
    /// window (capped at ~48k tokens), where that exceeds the fixed size.
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_tps_scaled: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scaled_prompt_tokens: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

async fn ollama_chat(
    client: &reqwest::Client,
    url: &str,
    model: &str,
    content: &str,
    num_ctx: u32,
    num_predict: i32,
    timeout: Duration,
) -> Result<Value> {
    let response = client
        .post(format!("{url}/api/chat"))
        .timeout(timeout)
        .json(&json!({
            "model": model,
            "messages": [{ "role": "user", "content": content }],
            "stream": false,
            "options": { "num_ctx": num_ctx, "num_predict": num_predict, "temperature": 0 },
        }))
        .send()
        .await?
        .error_for_status()?;
    Ok(response.json().await?)
}

fn tps(count: &Value, duration_ns: &Value) -> f64 {
    let c = count.as_f64().unwrap_or(0.0);
    let d = duration_ns.as_f64().unwrap_or(0.0) / 1e9;
    if d > 0.0 {
        c / d
    } else {
        0.0
    }
}

/// A prompt of roughly `tokens` tokens with a unique prefix so Ollama's
/// prefix cache cannot answer for the measurement.
fn probe_prompt(tag: &str, tokens: usize) -> String {
    let filler = "Reference material to ignore. ";
    let chars = tokens * 7 / 2; // the provider's own ~3.5 chars/token
    let mut out = format!("Probe {tag}: read the following and reply with the word ok.\n");
    while out.len() < chars {
        out.push_str(filler);
    }
    out
}

async fn speed_probe(client: &reqwest::Client, url: &str, model: &str, w: u32) -> SpeedProbe {
    let mut probe = SpeedProbe {
        window: w,
        load_secs: 0.0,
        size_bytes: 0,
        size_vram_bytes: 0,
        served_ctx: None,
        gen_tps: 0.0,
        prompt_tps_fixed: 0.0,
        prompt_tps_scaled: None,
        scaled_prompt_tokens: None,
        error: None,
    };

    // Switch/load. Generous timeout: a 256k KV allocation is the slowest
    // thing this mode does.
    match ollama_chat(client, url, model, "hi", w, 2, Duration::from_secs(900)).await {
        Ok(r) => probe.load_secs = r["load_duration"].as_f64().unwrap_or(0.0) / 1e9,
        Err(e) => {
            probe.error = Some(format!(
                "load failed: {e:#} — a timeout here usually means another client holds \
                 Ollama's inference slot, not that the window cannot be served"
            ));
            return probe;
        }
    }

    // Let the runner settle before reading /api/ps: querying the instant
    // the request returns can catch the previous runner's numbers, which
    // is how a 262144 row once reported less resident memory than 131072.
    tokio::time::sleep(Duration::from_secs(2)).await;
    if let Ok(ps) = client.get(format!("{url}/api/ps")).send().await {
        if let Ok(v) = ps.json::<Value>().await {
            if let Some(m) = v["models"].as_array().and_then(|a| a.first()) {
                probe.size_bytes = m["size"].as_u64().unwrap_or(0);
                probe.size_vram_bytes = m["size_vram"].as_u64().unwrap_or(0);
                probe.served_ctx = m["context_length"]
                    .as_u64()
                    .or_else(|| m["details"]["context_length"].as_u64());
            }
        }
    }
    if let Some(served) = probe.served_ctx {
        if served != w as u64 {
            probe.error = Some(format!(
                "requested num_ctx {w} but Ollama is serving {served}; this row measures \
                 the served window, not the requested one"
            ));
        }
    }

    match ollama_chat(
        client,
        url,
        model,
        &format!("Probe {w} gen: write a long paragraph about the ocean."),
        w,
        256,
        Duration::from_secs(300),
    )
    .await
    {
        Ok(r) => probe.gen_tps = tps(&r["eval_count"], &r["eval_duration"]),
        Err(e) => probe.error = Some(format!("gen probe failed: {e:#}")),
    }

    match ollama_chat(
        client,
        url,
        model,
        &probe_prompt(&format!("{w}-fixed"), 6_000.min(w as usize * 3 / 5)),
        w,
        4,
        Duration::from_secs(600),
    )
    .await
    {
        Ok(r) => probe.prompt_tps_fixed = tps(&r["prompt_eval_count"], &r["prompt_eval_duration"]),
        Err(e) => probe.error = Some(format!("fixed prompt probe failed: {e:#}")),
    }

    let scaled = (w as usize * 3 / 5).min(48_000);
    if scaled > 9_000 {
        match ollama_chat(
            client,
            url,
            model,
            &probe_prompt(&format!("{w}-scaled"), scaled),
            w,
            4,
            Duration::from_secs(1_800),
        )
        .await
        {
            Ok(r) => {
                probe.prompt_tps_scaled =
                    Some(tps(&r["prompt_eval_count"], &r["prompt_eval_duration"]));
                probe.scaled_prompt_tokens =
                    Some(r["prompt_eval_count"].as_u64().unwrap_or(0) as usize);
            }
            Err(e) => probe.error = Some(format!("scaled prompt probe failed: {e:#}")),
        }
    }

    probe
}

/// The A/B compaction cost probe at one window: identical seeded history,
/// summarized with expansion off and then on.
fn compaction_probe_cases(w: u32) -> Vec<ModelCase> {
    // Overshoot the trigger. An earlier version seeded 95% of it, which
    // is below it by construction — the 32768 and 65536 cells duly
    // reported "no compaction ran" after half an hour of seeding each.
    // 130% leaves room for the estimator disagreeing with itself about
    // exactly where the threshold sits.
    let notes = ((approx_threshold(w) * 130 / 100) / NOTE_TOKENS).clamp(1, 45);
    let mut variants = Vec::new();
    for (mode, env) in [
        ("baseline", Vec::new()),
        (
            "expanded",
            vec![(
                "RUSTYKRAB_COMPACTION_EXPAND_CTX".to_string(),
                EXPAND_TARGET.to_string(),
            )],
        ),
    ] {
        // Ids are per-window and per-arm; leaked because ModelCase ids are
        // &'static str. A handful per run, bounded by the window list.
        let id: &'static str =
            Box::leak(format!("ablation-compaction-{w}-{mode}").into_boxed_str());
        let mut case = ModelCase::new(
            id,
            "Scaled compaction cost probe: history sized to this window's trigger",
        )
        .with_harness(model_suite::bounded_harness())
        .with_num_ctx(w)
        .ask(
            "Before we start: the staging cluster is named borealis. \
             Acknowledge in one sentence.",
        );
        for i in 0..notes {
            case = case.ask(model_suite::bulky_note(i + 1));
        }
        case = case
            .ask("What is the staging cluster called? Answer in one short sentence.")
            .expect(Assertion::NoRunError)
            .expect(Assertion::Compacted(true))
            .expect(Assertion::FinalContainsAny(s(&["borealis"])));
        case.extra_env = env;
        variants.push(case);
    }
    variants
}

pub async fn run(
    bin: &str,
    model: &str,
    ollama_url: &str,
    ctx_list: &[u32],
    reps: usize,
    case_filter: Option<&str>,
    quick: bool,
) -> Result<Value> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(1_800))
        .build()?;

    // The model's own maximum, for the report.
    let model_max: Option<u64> = match client
        .post(format!("{ollama_url}/api/show"))
        .json(&json!({ "model": model }))
        .send()
        .await
    {
        Ok(r) => r.json::<Value>().await.ok().and_then(|v| {
            v["model_info"]
                .as_object()?
                .iter()
                .find(|(k, _)| k.ends_with(".context_length"))
                .and_then(|(_, v)| v.as_u64())
        }),
        Err(_) => None,
    };

    let mut windows: Vec<Value> = Vec::new();

    for &w in ctx_list {
        eprintln!("── window {w} ──");
        let probe = speed_probe(&client, ollama_url, model, w).await;
        eprintln!(
            "  load {:.1}s  gen {:.0} t/s  prompt {:.0} t/s  resident {:.1} GB",
            probe.load_secs,
            probe.gen_tps,
            probe.prompt_tps_fixed,
            probe.size_bytes as f64 / 1e9
        );
        if probe.error.is_some() && probe.gen_tps == 0.0 {
            // The window does not serve at all; record and move on.
            windows.push(json!({ "window": w, "speed": probe, "suite": null }));
            continue;
        }

        let selected: Vec<ModelCase> = model_suite::cases()
            .into_iter()
            .filter(|c| case_filter.is_none_or(|needle| c.id.contains(needle)))
            .filter(|c| !(quick && c.slow))
            .collect();
        let (mut suite, judge) =
            model_suite::run_cases(bin, model, ollama_url, reps, selected, Some(w)).await?;

        // Annotate compaction scenarios whose trigger the standard seed
        // cannot reach at this window. Their "failure" is geometry, not
        // behaviour, and counting it as accuracy would say the model got
        // worse at large windows when nothing of the kind happened.
        let unreachable = !standard_seed_fires(w);
        let mut annotated: Vec<Value> = Vec::new();
        for r in suite.drain(..) {
            let na = unreachable && FIRING_COMPACTION_SCENARIOS.contains(&r.id.as_str());
            let mut v = serde_json::to_value(&r)?;
            if na {
                v["not_applicable"] = json!("standard seed below this window's compaction trigger");
            }
            annotated.push(v);
        }

        // The compaction cost A/B, where the ceiling still lets the
        // trigger move with the window. Above the ceiling the threshold
        // stops growing, so re-measuring it would repeat the 65536 cell.
        let cost_probe = if (w as usize) <= COMPACTION_CEILING {
            let (reports, _) =
                // Same repetition count as the suite: with a thinking model
                // the summarizer's cost varies enormously call to call, and a
                // single sample of each arm cannot separate arm from noise.
                model_suite::run_cases(
                    bin,
                    model,
                    ollama_url,
                    reps,
                    compaction_probe_cases(w),
                    None,
                )
                .await?;
            Some(
                reports
                    .iter()
                    .map(serde_json::to_value)
                    .collect::<std::result::Result<Vec<_>, _>>()?,
            )
        } else {
            None
        };

        windows.push(json!({
            "window": w,
            "speed": probe,
            "judge": judge,
            "suite": annotated,
            "compaction_cost": cost_probe,
        }));
    }

    Ok(json!({
        "model": model,
        "model_max_context": model_max,
        "expand_target": EXPAND_TARGET,
        "compaction_ceiling": COMPACTION_CEILING,
        "windows": windows,
    }))
}

/// Render the report as a markdown summary.
pub fn render_markdown(report: &Value) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# Context-window ablation — {}\n\nmodel max context: {} · compaction ceiling: {} · expansion target: {}\n\n",
        report["model"].as_str().unwrap_or("?"),
        report["model_max_context"].as_u64().map(|v| v.to_string()).unwrap_or("unknown".into()),
        report["compaction_ceiling"],
        report["expand_target"],
    ));

    out.push_str("## Speed\n\n| window | served | load s | resident GB | gen t/s | prompt t/s (fixed) | prompt t/s (scaled) |\n|---|---|---|---|---|---|---|\n");
    for win in report["windows"].as_array().unwrap_or(&Vec::new()) {
        let sp = &win["speed"];
        // A probe that failed must say so, not render as a measured zero —
        // a busy inference slot and an unservable window both land here,
        // and "0 t/s" reads as data.
        if let Some(err) = sp["error"].as_str() {
            out.push_str(&format!(
                "| {} | probe failed: {err} ||||||\n",
                win["window"]
            ));
            continue;
        }
        out.push_str(&format!(
            "| {} | {} | {:.1} | {:.1} | {:.0} | {:.0} | {} |\n",
            win["window"],
            sp["served_ctx"]
                .as_u64()
                .map(|v| v.to_string())
                .unwrap_or("?".into()),
            sp["load_secs"].as_f64().unwrap_or(0.0),
            sp["size_bytes"].as_u64().unwrap_or(0) as f64 / 1e9,
            sp["gen_tps"].as_f64().unwrap_or(0.0),
            sp["prompt_tps_fixed"].as_f64().unwrap_or(0.0),
            sp["prompt_tps_scaled"]
                .as_f64()
                .map(|v| format!("{v:.0}"))
                .unwrap_or("—".into()),
        ));
    }

    out.push_str("\n## Accuracy (suite pass / applicable)\n\n| window | pass | fail | n/a by design | mean scenario ms |\n|---|---|---|---|---|\n");
    for win in report["windows"].as_array().unwrap_or(&Vec::new()) {
        let Some(suite) = win["suite"].as_array() else {
            out.push_str(&format!(
                "| {} | — window did not serve — ||||\n",
                win["window"]
            ));
            continue;
        };
        let na = suite
            .iter()
            .filter(|r| r.get("not_applicable").is_some())
            .count();
        let pass = suite
            .iter()
            .filter(|r| r["outcome"] == "pass" && r.get("not_applicable").is_none())
            .count();
        let fail = suite
            .iter()
            .filter(|r| r["outcome"] == "fail" && r.get("not_applicable").is_none())
            .count();
        let mean: u64 = {
            let ms: Vec<u64> = suite.iter().filter_map(|r| r["mean_ms"].as_u64()).collect();
            if ms.is_empty() {
                0
            } else {
                ms.iter().sum::<u64>() / ms.len() as u64
            }
        };
        out.push_str(&format!(
            "| {} | {pass} | {fail} | {na} | {mean} |\n",
            win["window"]
        ));
    }

    out.push_str("\n## Compaction cost (identical history; expansion off vs on)\n\n| window | baseline ms | baseline | expanded ms | expanded |\n|---|---|---|---|---|\n");
    for win in report["windows"].as_array().unwrap_or(&Vec::new()) {
        let Some(probe) = win["compaction_cost"].as_array() else {
            continue;
        };
        let cell = |mode: &str, field: &str| -> String {
            probe
                .iter()
                .find(|r| r["id"].as_str().is_some_and(|i| i.ends_with(mode)))
                .map(|r| match field {
                    "ms" => r["mean_ms"].to_string(),
                    // pass/fail alone hides a split verdict; show the rate.
                    _ => format!(
                        "{} {}/{}",
                        r["outcome"].as_str().unwrap_or("?"),
                        r["passes"],
                        r["runs"]
                    ),
                })
                .unwrap_or("—".into())
        };
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            win["window"],
            cell("baseline", "ms"),
            cell("baseline", "outcome"),
            cell("expanded", "ms"),
            cell("expanded", "outcome"),
        ));
    }
    out
}

// Quiet re-export check: the driver needs these from model_suite.
#[allow(unused_imports)]
use crate::model_suite::cases as _cases_visible;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thresholds_follow_the_window_until_the_ceiling() {
        // Small windows floor at a quarter; mid windows grow linearly;
        // beyond the ceiling the trigger stops moving — which is why the
        // cost probe does not re-run above it.
        assert_eq!(approx_threshold(4_096), 870);
        assert_eq!(approx_threshold(8_192), 1_305);
        assert!(approx_threshold(16_384) > 8_000);
        assert_eq!(approx_threshold(131_072), approx_threshold(262_144));
    }

    #[test]
    fn standard_seed_reachability_matches_the_thresholds() {
        assert!(standard_seed_fires(4_096));
        assert!(standard_seed_fires(8_192));
        assert!(
            !standard_seed_fires(16_384),
            "2.9k of seed cannot reach an 8.3k trigger"
        );
        assert!(!standard_seed_fires(65_536));
    }

    #[test]
    fn the_cost_probe_scales_its_seed_and_caps_it() {
        let small = compaction_probe_cases(8_192);
        let large = compaction_probe_cases(65_536);
        assert!(small[0].turns.len() < large[0].turns.len());
        assert!(large[0].turns.len() <= 47, "seed is capped");
        // The seed must exceed the trigger, not approach it: sizing to 95%
        // of the threshold meant compaction never fired at 32k and 64k.
        let seeded_tokens = (large[0].turns.len() - 2) * NOTE_TOKENS;
        assert!(
            seeded_tokens > approx_threshold(65_536),
            "seed {seeded_tokens} must exceed the {} trigger",
            approx_threshold(65_536)
        );
        // The two arms differ only in environment.
        assert_eq!(large[0].turns, large[1].turns);
        assert!(large[0].extra_env.is_empty());
        assert_eq!(large[1].extra_env[0].0, "RUSTYKRAB_COMPACTION_EXPAND_CTX");
    }
}
