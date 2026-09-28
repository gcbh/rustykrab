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
//! A second script checks the portable conversation shape (plan section
//! 12.1) in an ordinary run: three failed calls bring the reflection
//! prompt, a text reply the `task_complete` reminder, and both reach the
//! model as `[System notice]` user turns, with no system message stored
//! after the first and one tool block for the run.
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
/// The orchestration message that starts the notices run.
const NOTICES: &str = "e2e-toolsets: notices are user turns";
/// What the notices run's `task_complete` says.
const NOTICES_DONE: &str = "e2e-toolsets: gave up on the bad action";

/// This suite's part of the scripted daemon's script.
pub(crate) fn agent_script_scenarios() -> Vec<Value> {
    let call = |name: &str, arguments: Value| json!({ "toolCalls": [ { "name": name, "arguments": arguments } ] });
    // Rejected by schema validation, which is never retried unchanged.
    let bad = || call("cron", json!({ "action": "shred" }));
    vec![
        json!({
            "trigger": APPEND,
            "steps": [
                call("tools_list", json!({ "query": "scheduled tasks" })),
                call("tools_load", json!({ "names": ["cron"] })),
                call("cron", json!({ "action": "list" })),
                call("task_complete", json!({ "summary": DONE })),
            ],
        }),
        json!({
            "trigger": NOTICES,
            "steps": [
                bad(),
                bad(),
                bad(),
                { "text": "The scheduler rejects that action." },
                call("task_complete", json!({ "summary": NOTICES_DONE })),
            ],
        }),
    ]
}

/// The scripted scenarios, in run order.
pub(crate) fn scenarios() -> Vec<(Expected, (&'static str, ScenarioFn))> {
    let append: ScenarioFn = |ctx| Box::pin(append_path_keeps_the_tool_block(ctx));
    let notices: ScenarioFn = |ctx| Box::pin(notices_are_user_turns(ctx));
    vec![
        (
            Expected::Pass,
            ("toolsets/append-path-keeps-the-tool-block", append),
        ),
        (Expected::Pass, ("toolsets/notices-are-user-turns", notices)),
    ]
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

/// Plan section 12.1 in an ordinary run: the reflection prompt and the
/// `task_complete` reminder are `[System notice]` user turns, nothing after
/// the first message is a system message, and the tool block held.
async fn notices_are_user_turns(ctx: &Ctx) -> Result<()> {
    let conversation = ctx.create_conversation().await?;
    ctx.send(&conversation, NOTICES).await?;
    let run = Transcript::from_store(&ctx.db_path, &conversation)?;

    let rejected = run.calls_to("cron");
    ensure!(
        rejected.len() == 3 && rejected.iter().all(|c| c.failed),
        "expected three rejected cron calls, got {rejected:?}"
    );
    ensure!(
        run.late_system_messages == 0,
        "{} system message(s) stored after the first",
        run.late_system_messages
    );
    let reflection = run
        .notices
        .iter()
        .any(|n| n.contains("Multiple consecutive tool calls have failed"));
    let reminder = run
        .notices
        .iter()
        .any(|n| n.contains("did not call `task_complete`"));
    ensure!(
        reflection && reminder,
        "expected the reflection and the reminder as notices, got {:?}",
        run.notices
    );
    ensure!(
        run.final_text.contains(NOTICES_DONE),
        "the run did not end on its task_complete: {:?}",
        run.final_text
    );

    let blocks = crate::tool_blocks::read(&ctx.data_dir, &conversation);
    if let Some(why) = crate::tool_blocks::unchanged(&blocks, 5) {
        bail!("{why}");
    }
    Ok(())
}
