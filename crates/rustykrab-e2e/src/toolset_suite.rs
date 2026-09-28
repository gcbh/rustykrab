//! Scripted checks of Phase 2's append path (plan
//! `docs/plans/control-layer-and-worker-fleet.md`, section 12): what the
//! scripted daemon can observe of it without a model.
//!
//! The scripted provider takes calls to tools it was never declared, as
//! Ollama does, so the daemon runs its runs under the append binding. A
//! script searches the real catalog with `tools_list`, loads the found tool
//! again (gemma4's redundant load, which must be a no-op), calls it, and
//! ends with `task_complete`, which is appended too. The check reads the
//! run back from the store and the provider's `tool block sent` lines from
//! the daemon log: the definition arrived as text, the calls were
//! dispatched, and every request of the run declared the same tools array.
//!
//! Scenario 10 proper, the two default models on the distractor matrix,
//! is the model suite's (`late_binding.rs`).

use anyhow::{bail, ensure, Result};
use serde_json::{json, Value};

use crate::transcript::Transcript;
use crate::{Ctx, Expected, ScenarioFn};

/// The orchestration message that starts the scripted run.
const APPEND: &str = "e2e-toolsets: append a late tool";
/// What the scripted run's `task_complete` says.
const DONE: &str = "e2e-toolsets: listed the scheduled jobs";

/// This suite's part of the scripted daemon's script.
pub(crate) fn agent_script_scenarios() -> Vec<Value> {
    let call = |name: &str, arguments: Value| json!({ "toolCalls": [ { "name": name, "arguments": arguments } ] });
    vec![json!({
        "trigger": APPEND,
        "steps": [
            call("tools_list", json!({ "query": "scheduled tasks" })),
            call("tools_load", json!({ "names": ["cron"] })),
            call("cron", json!({ "action": "list" })),
            call("task_complete", json!({ "summary": DONE })),
        ],
    })]
}

/// The scripted scenarios, in run order.
pub(crate) fn scenarios() -> Vec<(Expected, (&'static str, ScenarioFn))> {
    let append: ScenarioFn = |ctx| Box::pin(append_path_keeps_the_tool_block(ctx));
    vec![(
        Expected::Pass,
        ("toolsets/append-path-keeps-the-tool-block", append),
    )]
}

async fn append_path_keeps_the_tool_block(ctx: &Ctx) -> Result<()> {
    let conversation = ctx.create_conversation().await?;
    ctx.send(&conversation, APPEND).await?;
    let run = Transcript::from_store(&ctx.db_path, &conversation)?;

    // The search appended the found tool's definition as text.
    let search = run.calls_to("tools_list");
    ensure!(
        search.len() == 1,
        "expected one search, got {}",
        search.len()
    );
    let found = search[0]
        .output
        .as_ref()
        .and_then(Value::as_str)
        .unwrap_or_default();
    ensure!(
        found.contains("callable now") && found.contains("\"name\":\"cron\""),
        "the search did not deliver cron's definition as text: {found}"
    );

    // Loading it again changed nothing and said so.
    let load = run.calls_to("tools_load");
    let load = load.first().and_then(|c| c.output.as_ref());
    ensure!(
        load.is_some_and(|o| o["already_callable"] == json!(["cron"]) && o["tools"] == json!([])),
        "a load of an appended tool was not a no-op: {load:?}"
    );

    // The appended tools were dispatched like declared ones.
    let cron = run.calls_to("cron");
    ensure!(
        cron.len() == 1 && !cron[0].failed,
        "the appended cron call did not run cleanly: {cron:?}"
    );
    ensure!(
        run.final_text.contains(DONE),
        "the appended task_complete did not end the run: {:?}",
        run.final_text
    );

    // One tools array for the whole run.
    let blocks = crate::tool_blocks::read(&ctx.data_dir, &conversation);
    if let Some(why) = crate::tool_blocks::unchanged(&blocks, 4) {
        bail!("{why}");
    }
    Ok(())
}
