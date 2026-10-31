//! Control-layer scenarios: section 15 of
//! `docs/plans/control-layer-and-worker-fleet.md`, as black-box drivers.
//!
//! Each scenario drives the surface of the plan's section 14 on the real
//! daemon: REST under `/api/work`, `/api/work/plan`, `/api/work/{id}/graph`,
//! `/api/work/archive`, `/api/workers` and `/api/questions`, the SSE
//! progress stream, the `work` CLI, the Telegram commands, and the scripted
//! agent's `work_file`, `work_plan` and `result_report` calls. It asserts on
//! what a user or a reviewer would see: statuses with their reasons and
//! origins, event kinds, the messages the Telegram stand-in received, and
//! the calls the GitHub stand-in logged.
//!
//! Every scenario is written to pass once its phase ships and is marked
//! `XFail` until then. Phases 1, 3, 4, 5 and 6 have shipped (see [`PROMOTED`]
//! and [`HELD_BACK`]). Every promoted scenario is required to pass.
//!
//! # Promotion
//!
//! Every scenario carries the phase of the plan's section 16 whose exit
//! turns it green. [`PROMOTED`] lists the phases that have shipped: adding
//! a phase to it flips exactly that phase's scenarios to must-pass, and
//! nothing else needs editing. The report's xpass rule forces the edit: a
//! scenario that starts passing before its phase is listed turns the suite
//! red.
//!
//! # How workers are scripted
//!
//! The daemon's scripted provider replays the script whose trigger is the
//! longest substring of the latest user message that matches. A local
//! worker's first user message is its brief, which carries the item's
//! objective, so an item's objective names the worker behaviour it wants
//! ([`W_SUCCEED`], [`W_TIMEOUT`], ...), and the orchestration conversation's
//! triggers (`e2e-control sNN: ...`) make the agent call `work_file`. The
//! scripts are in `control_suite/script.rs`. A script is static JSON, so it
//! cannot name an id the daemon assigns at run time; a scenario that needs
//! one (a worker's `supersedes` draft in 25) goes through REST instead.
//! Scenarios that need an external worker (2, 13, 17, 31) register a
//! Claude Code stand-in executable from `fixture_repo.rs`; scenario 8 boots
//! a second scripted daemon as the peer. The fleet scenarios outside the
//! plan's numbering (`fleet_scenarios`) boot their own daemon when their
//! premise is a daemon setting, such as `RUSTYKRAB_LOCAL_WORKER=off`. The
//! MCP bridge's (`control_suite/mcp.rs`) serves the gateway's router in
//! process instead and starts the daemon binary's `mcp-bridge` itself,
//! until the native worker mints the grants a booted daemon would hand it.
//!
//! # Wire assumptions
//!
//! The plan fixes the routes and the vocabulary (`rustykrab_core::work`),
//! not every wire shape. Where it is silent the assumption is made once,
//! in one helper of `control_suite/wire.rs`, so reconciling it with the
//! implementation is one edit:
//!
//! - `GET /api/work/{id}` returns the `WorkItem`, flat or under `item`, with
//!   `edges`, `events`, `evidence`, `lease` or `leases`, `ladder` and
//!   `last_error` beside it. `status` is `Status`'s serde form or the two
//!   store columns (`status`, `status_reason`). A lease lives only while its
//!   item is active, so a brief's inputs are read while it runs.
//! - `GET /api/work` lists open and recent items (an array, or under
//!   `items`) and filters on `kind`, `parent` and `include_closed`.
//! - `GET /api/work/{id}/graph` lists the tree's nodes depth first;
//!   `wire::graph_tree` nests them by parent.
//! - Every filing answers with `PlanOutcome`. A REST filing may carry
//!   `origin_conversation_id`, so the controller reports to that
//!   conversation's channel as it does for the conversation that called
//!   `work_file`.
//! - User commands go over REST as `POST /api/work/{id}/cancel` (whose
//!   reply lists what was cancelled and what had already finished) and
//!   `POST /api/questions/{id}/answer`, and through the Telegram commands of
//!   section 14.2 (`/approve`, `/reject` with a full item id) once channel
//!   commands ship with the plan previews of Phase 4.
//! - Worker runs are conversations stored in `messages` like any other, so
//!   a worker's brief and first step can be read back from the store.
//! - Not named by the plan, and served since Phase 1:
//!   `GET /api/work/events` (the SSE stream), `POST /api/work/import` (the
//!   delivery import of 26, `{ "manifest": StackManifest }`),
//!   `GET /api/work/archive?search=` (the archive view of 28, rows under
//!   `archived`). Served since Phase 3: `GET /api/workers` (rows under
//!   `workers`, each with its `routing_record` keyed by work class) and
//!   `POST /api/workers` with `kind`, `name`, `repos` and `command` (the
//!   Claude Code executable). Served since Phase 6: `POST /api/work/evaluate`
//!   (run the nightly evaluation now: decisions synced back, metrics,
//!   criteria, proposals, projection), `GET /api/work/metrics` (the section
//!   1.1 metrics of 16, under `metrics`), `capability: build | acquire` on a
//!   capability draft (27), `subject` and `review_tier` on a proposal
//!   draft (12), and the GitHub adapter reading `RUSTYKRAB_GITHUB_API_BASE`,
//!   `RUSTYKRAB_GITHUB_REPO` and `RUSTYKRAB_GITHUB_TOKEN` (`surface.rs`).
//!   Served since Phase 5: `POST /api/workers` with `kind: peer`,
//!   `base_url` and `token` (a peer, leasable once the registry has read its
//!   node's `GET /api/node` advertisement), and on the peer daemon the
//!   `delegated_tasks` columns `work_item_id`, `status` and `attempts`,
//!   which scenario 8 reads from the peer's store.

mod mcp;
mod script;
mod wire;

use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, ensure, Context, Result};
use chrono::Utc;
use serde_json::{json, Value};

use rustykrab_core::work::{
    BlockedReason, CancelReason, EdgeKind, ErrorClass, EventKind, PlanOutcome, RejectionReason,
    Rung, Status,
};

use crate::fixture_repo::{
    stack_manifest, ClaudeCodeStandIn, FixtureRepo, CLAIM_WRONGLY, INVESTIGATION_CLAIM_WRONGLY,
    INVESTIGATION_DAEMON_FAILURE, INVESTIGATION_FAIL_IN_DAEMON, INVESTIGATION_LANDED,
    INVESTIGATION_LAND_THE_FIX, INVESTIGATION_NOT_DONE, INVESTIGATOR_CAPTURES,
    KEEPS_ITS_OWN_TRACKER,
};
use crate::transcript::Transcript;
use crate::{Ctx, Expected, ScenarioFn};

pub(crate) use script::agent_script_scenarios;
use script::*;
use wire::*;

// ── the catalog and its promotion rule ───────────────────────────────

/// A phase of the plan's section 16. `ThreeExit` is scenario 13, which
/// Phase 3's exit criterion names separately: the ladder's build rung
/// uses the first external worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    One,
    Three,
    ThreeExit,
    Four,
    Five,
    Six,
}

/// The phases that have shipped. Their scenarios must pass; every other
/// scenario is `XFail`. Promoting a phase is this one edit.
const PROMOTED: &[Phase] = &[
    Phase::One,
    Phase::Three,
    Phase::ThreeExit,
    Phase::Four,
    Phase::Five,
    Phase::Six,
];

/// Scenarios of a promoted phase that stay `XFail`, each with why. An
/// entry here is a known gap in a shipped phase, named rather than hidden;
/// the report still runs it, and it turns the suite red the day it passes,
/// so it leaves this list the same day.
const HELD_BACK: &[(u8, &str)] = &[];

/// Plan scenarios another suite owns, with where. Read by the catalog
/// test that holds every plan number to exactly one home.
#[cfg_attr(not(test), allow(dead_code))]
const COVERED_ELSEWHERE: &[(u8, &str)] = &[(
    10,
    "the model suite: gemma4 and qwen3.8 on the distractor-catalog matrix through tools_load (Phase 2)",
)];

struct Entry {
    /// The plan's scenario number, which the id also carries; the catalog
    /// tests check the two agree and that every number has one home.
    #[cfg_attr(not(test), allow(dead_code))]
    number: u8,
    phase: Phase,
    id: &'static str,
    run: ScenarioFn,
}

impl Entry {
    fn expected(&self) -> Expected {
        let held = HELD_BACK.iter().any(|(n, _)| *n == self.number);
        if PROMOTED.contains(&self.phase) && !held {
            Expected::Pass
        } else {
            Expected::XFail
        }
    }
}

/// One row per plan scenario: number, phase, id, driver. A table rather
/// than a `vec!` so it stays one line a scenario.
macro_rules! catalog {
    ($($number:literal $phase:ident $id:literal $run:ident;)*) => {
        vec![$(Entry {
            number: $number,
            phase: Phase::$phase,
            id: $id,
            run: |ctx| Box::pin($run(ctx)),
        },)*]
    };
}

/// Stable scenario catalog, in plan order. The ids are an external
/// interface for CI filters (`--case control/`) and for promotion; rename
/// them only as an intentional migration.
///
/// One row departs from section 16's table: scenario 4 is listed under
/// Phase 5 there, but this driver restarts the daemon under a local lease,
/// which Phase 1's resume (plan 6.7, the graph form of which is scenario
/// 32) already satisfies. It is promoted with Phase 1 rather than left to
/// xpass; its peer half, a restart on either side of a peer's run, is in
/// scenario 8's driver.
#[rustfmt::skip]
fn catalog() -> Vec<Entry> {
    catalog! {
        1  One       "control/01-personal-task-filed-leased-completed-reported" s01;
        2  Three     "control/02-claude-code-commit-verified-and-false-claim-caught" s02;
        3  Four      "control/03-needs-decision-question-resumes-the-item" s03;
        4  One       "control/04-restart-mid-lease-resumes-or-returns-to-ready" s04;
        5  Four      "control/05-stalled-worker-repaired-with-first-attempts-evidence" s05;
        6  One       "control/06-same-calendar-writers-never-run-together" s06;
        7  Six       "control/07-dreaming-proposal-from-verifiable-signal-only" s07;
        8  Five      "control/08-peer-activates-required-tools-and-returns-typed-result" s08;
        9  One       "control/09-expired-item-told-and-never-run" s09;
        11 One       "control/11-unloaded-tool-says-load-it-unconfigured-mcp-needs-tool" s11;
        12 Six       "control/12-self-modifying-proposal-needs-highest-review-tier" s12;
        13 ThreeExit "control/13-missing-tool-built-then-original-resumes-unasked" s13;
        14 One       "control/14-unknown-failure-files-internal-item-then-classifies" s14;
        15 Six       "control/15-defaulted-surfaced-question-flagged-as-avoidable" s15;
        16 Six       "control/16-expectation-metrics-nightly-regression-proposal" s16;
        17 Three     "control/17-code-routing-record-and-escalation-to-claude-code" s17;
        18 Four      "control/18-planner-cycle-rejected-whole-then-corrected" s18;
        19 One       "control/19-validation-bounds-over-decomposition" s19;
        20 One       "control/20-chain-failure-climbs-ladder-then-cascades-once" s20;
        21 One       "control/21-plan-b-runs-on-failure-and-cancels-on-success" s21;
        22 One       "control/22-fan-in-brief-carries-only-named-inputs" s22;
        23 One       "control/23-parent-cancel-cascades-and-reply-lists-both" s23;
        24 One       "control/24-expiry-holds-blocks-releases-waits-for-cascades-parent" s24;
        25 One       "control/25-supersede-replan-active-refused-rate-limited" s25;
        26 One       "control/26-delivery-import-builds-layered-graph-without-planner" s26;
        27 Six       "control/27-projection-rule-by-kind" s27;
        28 One       "control/28-aging-compacts-closed-items-to-archive" s28;
        29 Four      "control/29-approval-threshold-holds-previews-and-releases" s29;
        30 One       "control/30-cron-firing-is-a-work-item-serialised-with-turns" s30;
        31 Three     "control/31-claude-code-discovered-draft-files-one-item" s31;
        32 One       "control/32-restart-mid-graph-rederives-and-sends-once" s32;
    }
}

pub(crate) fn scenarios() -> Vec<(Expected, (&'static str, ScenarioFn))> {
    let mut scenarios: Vec<_> = catalog()
        .into_iter()
        .map(|entry| (entry.expected(), (entry.id, entry.run)))
        .collect();
    scenarios.extend(fleet_scenarios());
    scenarios
}

/// Fleet behaviour outside the plan's section 15, so outside the numbered
/// catalog and its promotion rule: each is must-pass from the day it lands.
fn fleet_scenarios() -> Vec<(Expected, (&'static str, ScenarioFn))> {
    vec![
        (
            Expected::Pass,
            (
                "control/agent-monitor-verification-metrics-and-cli",
                |ctx| Box::pin(agent_monitor(ctx)),
            ),
        ),
        (
            Expected::Pass,
            (
                "control/local-worker-off-leases-to-the-external-worker",
                |ctx| Box::pin(local_worker_off(ctx)),
            ),
        ),
        (
            Expected::Pass,
            ("control/two-daemons-one-data-dir-only-one-ticks", |ctx| {
                Box::pin(two_daemons_one_data_dir(ctx))
            }),
        ),
        (
            Expected::Pass,
            ("control/local-worker-run-leaves-a-session-log", |ctx| {
                Box::pin(local_worker_session_log(ctx))
            }),
        ),
        (
            Expected::Pass,
            ("control/investigation-kit-for-a-recurring-failure", |ctx| {
                Box::pin(investigation_kit(ctx))
            }),
        ),
        (
            Expected::Pass,
            (
                "control/fix-item-lands-a-commit-in-the-daemons-own-source",
                |ctx| Box::pin(own_source_fix(ctx)),
            ),
        ),
        (
            Expected::Pass,
            (
                "control/bounded-recovery-repeated-failure-trips-the-breaker-once",
                |ctx| Box::pin(breaker_trips_once(ctx)),
            ),
        ),
        (
            Expected::Pass,
            (
                "control/usage-only-work-passes-old-estimates-and-keeps-real-decisions",
                |ctx| Box::pin(usage_only_decision_resumes(ctx)),
            ),
        ),
        (
            Expected::Pass,
            (
                "control/capability-proof-a-build-counts-once-its-operation-works-on-the-executor",
                |ctx| Box::pin(capability_proof(ctx)),
            ),
        ),
        (
            Expected::Pass,
            (
                "control/native-agent-calls-a-granted-host-tool-through-the-mcp-bridge",
                |ctx| Box::pin(mcp::native_agent_through_the_bridge(ctx)),
            ),
        ),
    ]
}

/// The monitor must report the daemon's verified outcome, rather than treating a
/// worker's completion claim as evidence. It also exercises the actual CLI.
async fn agent_monitor(ctx: &Ctx) -> Result<()> {
    let task = file(
        ctx,
        draft("personal", "Monitor verified work", &tag(0), W_SUCCEED),
    )
    .await?;
    let detail = wait_status(ctx, &task, Status::Done).await?;
    let snapshot = get(ctx, "/api/monitor?limit=500").await?;
    let rows = snapshot["work"]["items"]
        .as_array()
        .context("monitor items")?;
    let row = rows
        .iter()
        .find(|r| r["item"]["id"] == task)
        .context("completed task visible")?;
    ensure!(
        row["item"]["status"]["status"] == "done",
        "monitor reports durable status"
    );
    ensure!(
        row["last_worker"].is_string(),
        "monitor keeps worker after lease release"
    );
    ensure!(row["lease"].is_null(), "completed work has no live lease");
    ensure!(
        row["evidence_count"].as_u64().unwrap_or(0) > 0,
        "monitor keeps result evidence"
    );
    ensure!(
        row["verified_evidence_count"].as_u64().unwrap_or(0) > 0,
        "monitor distinguishes verification from a claim: {row}"
    );
    ensure!(
        !detail["evidence"].as_array().unwrap().is_empty(),
        "detail also contains evidence"
    );
    ensure!(
        snapshot["controller"]["last_tick"].is_string(),
        "monitor observes real controller execution"
    );
    ensure!(
        !snapshot["workers"]
            .as_array()
            .context("workers")?
            .is_empty(),
        "monitor observes agents"
    );
    let output = ctx.cli(&["monitor", "--json", "--check"])?;
    let cli: Value = serde_json::from_slice(&output.stdout)?;
    ensure!(
        output.status.success() == (cli["health"] == "healthy"),
        "health-check exit agrees with its observation"
    );
    ensure!(
        cli["work"]["captured_at"].is_string(),
        "monitor CLI reads a real snapshot"
    );
    let metrics = ctx
        .client
        .get(ctx.url("/api/monitor/metrics"))
        .bearer_auth(crate::AUTH_TOKEN)
        .header("Origin", &ctx.base)
        .send()
        .await?;
    ensure!(metrics.status().is_success(), "metrics returns success");
    let text = metrics.text().await?;
    ensure!(
        text.contains("rustykrab_work_items") && text.contains("rustykrab_controller_runs"),
        "machine monitoring exposes work and execution"
    );
    let shell = ctx.client.get(ctx.url("/monitor.html")).send().await?;
    ensure!(
        shell.status().is_success() && shell.text().await?.contains("Work manager"),
        "dashboard is served"
    );
    let script = ctx.client.get(ctx.url("/monitor.js")).send().await?;
    ensure!(
        script.status().is_success() && script.text().await?.contains("/api/monitor"),
        "dashboard script is served"
    );
    Ok(())
}

// ── Phase 1: work items and the controller skeleton ──────────────────

/// Scenario 1: A task is filed, leased to a local worker with its toolset
/// pre-activated, completed, and reported through Telegram with an execution pointer.
async fn s01(ctx: &Ctx) -> Result<()> {
    let progress = EventStream::open(ctx).await?;
    let conversation = telegram_thread(ctx, 101).await?;
    telegram_say(ctx, 101, S01_FILE).await?;
    let filed = wait_titled(ctx, S01_TITLE).await?;
    let task = id(&filed);
    ensure!(
        body(&filed)["kind"] == "task",
        "the task is filed without an enforced category: {}",
        body(&filed)["kind"]
    );
    ensure!(
        body(&filed)["origin_conversation_id"] == conversation.as_str(),
        "the task names the Telegram conversation that filed it"
    );
    let finished = wait_status(ctx, &task, Status::Done).await?;
    let worker =
        worker_of(&finished).ok_or_else(|| anyhow!("the finished task names no worker"))?;
    // A local worker's run is a conversation in this daemon's own store
    // that carries the brief and ends with `result_report`. (The registry
    // that would list the worker's kind is `GET /api/workers`, Phase 3.)
    let runs = worker_runs(ctx, S01_WORKER)?;
    let first = runs.first().ok_or_else(|| {
        anyhow!("no local worker run carries the task's brief (the lease went to {worker})")
    })?;
    let active = active_at_first_step(first);
    ensure!(
        active.iter().any(|tool| tool == "caldav"),
        "caldav was active at the worker's first step, before any load: {active:?}"
    );
    ensure!(
        evidence_text(&finished).contains(S01_EVIDENCE),
        "the confirmation is attached as evidence: {}",
        evidence_text(&finished)
    );
    let reports = wait_told(ctx, "Book the dentist").await?;
    let pointer = format!("Details: /work {task}");
    let notification = reports
        .iter()
        .find(|m| m.contains(&pointer))
        .ok_or_else(|| anyhow!("the Telegram notification links to its execution: {reports:?}"))?;
    ensure!(
        notification.contains("Booked the dentist"),
        "the notification explains the outcome: {notification}"
    );
    ensure!(
        !notification.contains(S01_EVIDENCE),
        "receipts belong in execution details: {notification}"
    );
    let detail = get(ctx, &format!("{WORK}/{task}")).await?;
    ensure!(
        evidence_text(&detail).contains(S01_EVIDENCE),
        "the notification's work pointer resolves to its full evidence"
    );
    let frames = progress.about(&task);
    ensure!(
        frames
            .iter()
            .any(|(kind, data)| kind == EventKind::Lease.as_str() || data["kind"] == "lease"),
        "the progress stream carried the lease: {frames:?}"
    );
    ensure!(
        frames
            .iter()
            .any(|(_, data)| moved_to(data) == Some(Status::Done)),
        "the progress stream carried the move to done: {frames:?}"
    );
    Ok(())
}

/// Scenario 6: Two items that write the same calendar are never running together.
async fn s06(ctx: &Ctx) -> Result<()> {
    let tag = tag(6);
    let calendar = format!("calendar {tag}");
    let mut writers = Vec::new();
    for name in [
        "Add the dentist to the calendar",
        "Add the school run to the calendar",
    ] {
        let mut writer = draft("personal", name, &tag, W_HOLD);
        writer["writable_resources"] = json!([calendar]);
        writers.push(file(ctx, writer).await?);
    }
    let first = active_span(&wait_status(ctx, &writers[0], Status::Done).await?)?;
    let second = active_span(&wait_status(ctx, &writers[1], Status::Done).await?)?;
    ensure!(
        first.1 <= second.0 || second.1 <= first.0,
        "the two writers of one calendar ran together: {first:?} and {second:?}"
    );
    Ok(())
}

/// Scenario 9: An item expires; the user is told; nothing runs.
async fn s09(ctx: &Ctx) -> Result<()> {
    let tag = tag(9);
    let conversation = telegram_thread(ctx, 109).await?;
    let mut permit = draft("personal", "Renew the parking permit", &tag, W_SUCCEED);
    permit["trigger"] = trigger_at(3600);
    permit["expires_at"] = json!(later(3));
    permit["origin_conversation_id"] = json!(conversation);
    let permit = file(ctx, permit).await?;
    let expired = wait_status(ctx, &permit, Status::Expired).await?;
    // The originating conversation may carry the controller's notice; any
    // other conversation naming the item would be a worker's brief.
    let briefed = conversations_mentioning(ctx, &tag)?
        .into_iter()
        .filter(|c| c != &conversation)
        .count();
    ensure!(
        !was_leased(&expired) && briefed == 0,
        "nothing ran: the expired item was leased or briefed to a worker"
    );
    wait_told(ctx, &title(&expired)).await?;
    Ok(())
}

/// Scenario 11: `work_file` with `required_tools` that are registered but unloaded is
/// rejected with "load it"; with an unconfigured MCP server it is accepted
/// as `needs_tool`.
async fn s11(ctx: &Ctx) -> Result<()> {
    let conversation = ctx.create_conversation().await?;
    ctx.send(&conversation, S11_UNLOADED).await?;
    let run = Transcript::from_store(&ctx.db_path, &conversation)?;
    let answer = run.outputs_of("work_file");
    ensure!(
        answer.to_lowercase().contains("load it"),
        "work_file naming the registered but unloaded caldav is rejected with \"load it\": {answer}"
    );
    ensure!(
        find_titled(ctx, S11_UNLOADED_TITLE).await?.is_none(),
        "the rejected draft was not filed"
    );
    ctx.send(&conversation, S11_MCP).await?;
    let filed = wait_titled(ctx, S11_MCP_TITLE).await?;
    ensure!(
        status(&filed)? == Status::Blocked(BlockedReason::NeedsTool),
        "the draft naming an unconfigured MCP server is accepted as needs_tool: {}",
        shown(&filed)
    );
    Ok(())
}

/// Scenario 14: A failure no classifier recognises is recorded as `unknown`, an
/// `internal` item is filed with the raw evidence, and after it lands the
/// same failure is classified on replay.
async fn s14(ctx: &Ctx) -> Result<()> {
    let tag = tag(14);
    let ledger = file(
        ctx,
        draft("personal", "Reconcile the ledger", &tag, W_UNRECOGNISED),
    )
    .await?;
    let error = wait_error(ctx, &ledger).await?;
    ensure!(
        error.class == ErrorClass::Unknown,
        "the unrecognised failure is recorded as unknown, not {}",
        error.class.as_str()
    );
    let internal = wait_listed(
        ctx,
        "kind=internal",
        "an internal item for the failure",
        |v| v.to_string().contains(&ledger),
    )
    .await?;
    ensure!(
        internal.to_string().contains(RAW_FAILURE),
        "the internal item carries the raw evidence: {}",
        excerpt(&internal.to_string())
    );
    wait_status(ctx, &id(&internal), Status::Done).await?;
    let replay = file(
        ctx,
        draft(
            "personal",
            "Reconcile the ledger again",
            &tag,
            W_UNRECOGNISED,
        ),
    )
    .await?;
    let again = wait_error(ctx, &replay).await?;
    ensure!(
        again.class != ErrorClass::Unknown,
        "once the internal item landed, the same failure is classified on replay"
    );
    Ok(())
}

/// Scenario 19: Validation bounds over-decomposition: twelve items for a three-step
/// errand is rejected with an event. Legacy token estimates and categories
/// do not prevent otherwise valid work; two `blocks`-chained steps that add
/// nothing to each other are accepted as small execution slices without
/// a split warning. Driven by a scripted caller until the planner
/// exists.
async fn s19(ctx: &Ctx) -> Result<()> {
    let tag = tag(19);
    let mut errands = draft("personal", "Run the Saturday errands", &tag, W_SUCCEED);
    errands["trigger"] = not_yet();
    errands["budget"] = budget(100_000);
    let root = file(ctx, errands).await?;
    let step = |tmp: &str, name: &str, kind: &str, tokens: u64| {
        let mut item = draft(kind, name, &tag, W_SUCCEED);
        item["tmp"] = json!(tmp);
        item["budget"] = budget(tokens);
        item
    };
    let under = |items: Vec<Value>, edges: Vec<Value>| json!({ "root": root, "items": items, "edges": edges, "rationale": "the errands" });

    let twelve = (1..=12)
        .map(|n| {
            step(
                &format!("step{n}"),
                &format!("Errand step {n}"),
                "personal",
                5_000,
            )
        })
        .collect();
    let too_many = rejected(plan(ctx, under(twelve, vec![])).await?)?;
    ensure!(
        reasons(&too_many).contains(&RejectionReason::TooManyItems),
        "twelve items for a three-step errand are rejected with too_many_items: {:?}",
        reasons(&too_many)
    );
    ensure!(
        children(ctx, &root).await?.is_empty(),
        "rejected graphs store nothing"
    );
    let rejections =
        Value::Array(events_of(&item(ctx, &root).await?, EventKind::Rejection)).to_string();
    ensure!(
        rejections.contains(RejectionReason::TooManyItems.as_str()),
        "the structural rejection remains an event dreaming can count: {rejections}"
    );
    let shops = vec![
        step("groceries", "Buy the groceries", "personal", 80_000),
        step("paint", "Buy the paint", "personal", 80_000),
    ];
    accepted(plan(ctx, under(shops, vec![])).await?)?;
    ensure!(
        children(ctx, &root).await?.len() == 2,
        "child estimates do not reserve or exhaust a parent's capacity"
    );
    let fix = vec![step("fix", "Fix the errand app", "code", 5_000)];
    accepted(plan(ctx, under(fix, vec![])).await?)?;
    let fixed = children(ctx, &root).await?;
    ensure!(fixed.len() == 3, "all three valid steps were accepted");
    ensure!(
        body(&item(ctx, fixed.last().unwrap()).await?)["kind"] == "task",
        "a legacy category cannot reject an otherwise valid planned task"
    );

    let mut parcels = draft("personal", "Post the parcels", &tag, W_SUCCEED);
    parcels["trigger"] = not_yet();
    let parcels = file(ctx, parcels).await?;
    let chained = json!({
        "root": parcels,
        "items": [
            step("weigh", "Weigh the parcels", "personal", 5_000),
            step("send", "Send the parcels", "personal", 5_000),
        ],
        "edges": [ { "item": { "tmp": "send" }, "kind": "blocks", "depends_on": { "tmp": "weigh" } } ],
        "rationale": "weigh, then send",
    });
    let split = accepted(plan(ctx, chained).await?)?;
    ensure!(
        !split
            .warnings
            .iter()
            .any(|w| w.check == rustykrab_core::work::WarningCheck::SequentialSplit),
        "small sequential execution slices are allowed without a split warning: {:?}",
        split.warnings
    );
    let mut warned = events_of(&item(ctx, &parcels).await?, EventKind::Warning);
    for id in split.ids.values() {
        warned.extend(events_of(&item(ctx, id).await?, EventKind::Warning));
    }
    ensure!(
        !Value::Array(warned)
            .to_string()
            .contains("sequential_split"),
        "ordinary sequential chunks do not produce a split warning event"
    );
    Ok(())
}

/// Scenario 20: In a chain a, b, c under P, b fails: its ladder runs first and
/// nothing downstream moves; then everything behind b is
/// `blocked(upstream_failed)` with origin b, a stays done, P rolls up to
/// blocked naming b, and the user gets one message.
async fn s20(ctx: &Ctx) -> Result<()> {
    let tag = tag(20);
    let conversation = telegram_thread(ctx, 120).await?;
    let filed = file_graph(
        ctx,
        vec![
            root_node("P", "Host the dinner party", &tag),
            node("a", "P", "personal", "Choose the menu", &tag, W_SUCCEED),
            node("b", "P", "personal", "Order the groceries", &tag, W_TIMEOUT),
            node("c", "P", "personal", "Cook the dinner", &tag, W_SUCCEED),
            node(
                "d",
                "P",
                "personal",
                "Send the thank-you notes",
                &tag,
                W_SUCCEED,
            ),
        ],
        vec![
            edge("b", EdgeKind::Blocks, "a"),
            edge("c", EdgeKind::Blocks, "b"),
            edge("d", EdgeKind::Blocks, "c"),
        ],
        Some(&conversation),
    )
    .await?;
    let [p, a, b, c, d] = ids(&filed, ["P", "a", "b", "c", "d"])?;
    let failed = wait_status(ctx, &b, Status::Failed).await?;
    for rung in [Rung::Retry, Rung::Repair, Rung::SwitchWorker] {
        ensure!(
            climbed(&failed, rung),
            "b's ladder ran {} as an event on b: {:?}",
            rung.as_str(),
            rungs(&failed)
        );
    }
    let ladder_end = rungs(&failed)
        .iter()
        .filter_map(|(_, at)| *at)
        .max()
        .ok_or_else(|| anyhow!("b's rungs carry no times"))?;
    for held in [&c, &d] {
        let view = wait_cascade(
            ctx,
            held,
            Status::Blocked(BlockedReason::UpstreamFailed),
            &b,
        )
        .await?;
        ensure!(!was_leased(&view), "{} never ran", title(&view));
        let moved_early = events(&view)
            .iter()
            .filter(|e| moved_to(e).is_some_and(|s| s != Status::Queued))
            .filter_map(|e| time(&e["at"]))
            .any(|at| at < ladder_end);
        ensure!(
            !moved_early,
            "{} changed while b's ladder was still climbing",
            title(&view)
        );
    }
    expect_status(ctx, &a, Status::Done, "a stays done").await?;
    let parent = wait_for(ctx, &p, "P rolls up to blocked", |v| {
        status(v).is_ok_and(|s| s.name() == "blocked")
    })
    .await?;
    ensure!(
        origin(&parent).as_deref() == Some(b.as_str()),
        "P's roll-up names b: {:?}",
        origin(&parent)
    );
    let message = told_once(ctx, &tag).await?;
    ensure!(
        message.contains(&title(&parent)),
        "the message names P: {message}"
    );
    ensure!(
        message.contains(&title(&failed)),
        "the message names b: {message}"
    );
    let reached = rungs(&failed)
        .into_iter()
        .max_by_key(|(_, at)| *at)
        .map(|(name, _)| name)
        .unwrap_or_default();
    ensure!(
        mentions_rung(&message, &reached),
        "the message names the rung b reached ({reached}): {message}"
    );
    for waiting in [&c, &d] {
        let name = title(&item(ctx, waiting).await?);
        ensure!(
            message.contains(&name),
            "the message names {name}, waiting on b"
        );
    }
    Ok(())
}

/// Scenario 21: b is `conditional_on_failure` on a. When a fails after its ladder, b
/// runs before anything is surfaced and the user gets only the final
/// report; when a succeeds, b ends `cancelled(cascade)` without a lease.
async fn s21(ctx: &Ctx) -> Result<()> {
    let conversation = telegram_thread(ctx, 121).await?;
    let first = tag(21);
    let filed = file_graph(
        ctx,
        vec![
            root_node("P", "Book a table for Friday", &first),
            node(
                "a",
                "P",
                "personal",
                "Book the table online",
                &first,
                W_TIMEOUT,
            ),
            node(
                "b",
                "P",
                "personal",
                "Book the table by email",
                &first,
                W_SUCCEED,
            ),
        ],
        vec![edge("b", EdgeKind::ConditionalOnFailure, "a")],
        Some(&conversation),
    )
    .await?;
    let [p, a, b] = ids(&filed, ["P", "a", "b"])?;
    let failed = wait_status(ctx, &a, Status::Failed).await?;
    let plan_b = wait_status(ctx, &b, Status::Done).await?;
    ensure!(
        leased_at(&plan_b)? >= closed_at(&failed)?,
        "plan B ran only once a had failed"
    );
    ensure!(
        !climbed(&failed, Rung::Surface),
        "a's failure was surfaced instead of releasing plan B"
    );
    wait_status(ctx, &p, Status::Done).await?;
    let report = told_once(ctx, &first).await?;
    ensure!(
        report.contains(&title(&plan_b)),
        "the one message is the final report, naming the plan B that did it: {report}"
    );

    let second = tag(21);
    let filed = file_graph(
        ctx,
        vec![
            root_node("P", "Book a table for Saturday", &second),
            node(
                "a",
                "P",
                "personal",
                "Book the table online",
                &second,
                W_SUCCEED,
            ),
            node(
                "b",
                "P",
                "personal",
                "Book the table by email",
                &second,
                W_SUCCEED,
            ),
        ],
        vec![edge("b", EdgeKind::ConditionalOnFailure, "a")],
        Some(&conversation),
    )
    .await?;
    let [_, a, b] = ids(&filed, ["P", "a", "b"])?;
    wait_status(ctx, &a, Status::Done).await?;
    let unused = wait_cascade(ctx, &b, Status::Cancelled(CancelReason::Cascade), &a).await?;
    ensure!(
        !was_leased(&unused),
        "plan B was leased although a succeeded"
    );
    Ok(())
}

/// Scenario 22: c is blocked by a and b and takes `inputs_from: [a, b]`: it is not
/// ready until both are done, and its brief carries both items' evidence as
/// typed `artifact_refs` and nothing from a sibling it did not name.
async fn s22(ctx: &Ctx) -> Result<()> {
    let tag = tag(22);
    let mut chooser = node(
        "c",
        "P",
        "personal",
        "Choose the venue and caterer",
        &tag,
        W_READ_INPUTS,
    );
    chooser["inputs_from"] = json!([{ "tmp": "a" }, { "tmp": "b" }]);
    let filed = file_graph(
        ctx,
        vec![
            root_node("P", "Plan the reunion", &tag),
            node(
                "a",
                "P",
                "research",
                "Find the venue options",
                &tag,
                W_DOC_ALPHA,
            ),
            node(
                "b",
                "P",
                "research",
                "Find the catering options",
                &tag,
                W_DOC_BETA_SLOW,
            ),
            node(
                "s",
                "P",
                "research",
                "Find the band options",
                &tag,
                W_DOC_GAMMA,
            ),
            chooser,
        ],
        vec![
            edge("c", EdgeKind::Blocks, "a"),
            edge("c", EdgeKind::Blocks, "b"),
        ],
        None,
    )
    .await?;
    let [_, a, b, s, c] = ids(&filed, ["P", "a", "b", "s", "c"])?;
    wait_status(ctx, &a, Status::Done).await?;
    let (b_now, c_now) = (item(ctx, &b).await?, item(ctx, &c).await?);
    if status(&b_now)? != Status::Done {
        ensure!(
            status(&c_now)? == Status::Queued,
            "c is not ready while b is open: {}",
            shown(&c_now)
        );
    }
    // The inputs block is read at lease time, from the live lease: a lease
    // lives only while its item is active.
    let leased = wait_for(ctx, &c, "c is leased", |v| {
        is_active(v) || status(v).is_ok_and(|s| s.is_closed())
    })
    .await?;
    let chosen = wait_status(ctx, &c, Status::Done).await?;
    ensure!(
        leased_at(&chosen)? >= closed_at(&item(ctx, &b).await?)?,
        "c was leased before both inputs were done"
    );
    let inputs = lease_inputs(&leased);
    for (upstream, document) in [(&a, "e2e-control/alpha.md"), (&b, "e2e-control/beta.md")] {
        let input = inputs
            .iter()
            .find(|i| i["item"] == upstream.as_str())
            .ok_or_else(|| anyhow!("c's brief has no input from {upstream}: {inputs:?}"))?;
        let refs = input_refs(input)?;
        ensure!(
            refs.iter().any(|r| r.value == document),
            "the input from {upstream} carries {document} as a typed artifact ref: {refs:?}"
        );
    }
    let copied = Value::Array(inputs).to_string();
    ensure!(
        !copied.contains(s.as_str()) && !copied.contains("gamma"),
        "c's brief carries something from a sibling it did not name"
    );
    Ok(())
}

/// Scenario 23: P is cancelled with one child done, one running and two queued (one
/// with a child): the queued ones and the grandchild end
/// `cancelled(cascade)` unleased, the running child's lease is revoked and
/// it keeps its partial evidence, the done child keeps its status and
/// evidence, and the reply lists what was cancelled and what had finished.
async fn s23(ctx: &Ctx) -> Result<()> {
    let tag = tag(23);
    let deferred = |tmp: &str, parent: &str, name: &str| {
        let mut item = node(tmp, parent, "personal", name, &tag, W_SUCCEED);
        item["trigger"] = not_yet();
        item
    };
    let filed = file_graph(
        ctx,
        vec![
            root_node("P", "Organise the school fair", &tag),
            node("d1", "P", "personal", "Book the hall", &tag, W_SUCCEED),
            node("r", "P", "personal", "Print the posters", &tag, W_SLOW),
            deferred("q1", "P", "Order the bunting"),
            deferred("q2", "P", "Arrange the stalls"),
            deferred("g", "q2", "Book the cake stall"),
        ],
        vec![],
        None,
    )
    .await?;
    let [p, d1, r, q1, q2, g] = ids(&filed, ["P", "d1", "r", "q1", "q2", "g"])?;
    wait_status(ctx, &d1, Status::Done).await?;
    wait_for(ctx, &r, "r is running", is_active).await?;
    // The user's cancel is `work cancel`'s REST call; its reply lists what
    // was cancelled and what had already finished (section 14.2). The
    // channel's `/cancel` mirrors it once channel commands ship.
    let reply = post(ctx, &format!("{WORK}/{p}/cancel"), json!({})).await?;
    let listed = reply.to_string();
    for (name, id) in [
        ("Book the hall", &d1),
        ("Print the posters", &r),
        ("Order the bunting", &q1),
        ("Arrange the stalls", &q2),
        ("Book the cake stall", &g),
    ] {
        ensure!(
            listed.contains(id.as_str()) || listed.contains(&format!("{name} {tag}")),
            "the cancel reply does not name {name}: {}",
            excerpt(&listed)
        );
    }
    ensure!(
        reply["already_finished"]
            .as_array()
            .is_some_and(|done| done.iter().any(|f| f["id"] == d1.as_str())),
        "the reply lists the done child as already finished: {}",
        excerpt(&listed)
    );
    for queued in [&q1, &q2, &g] {
        let view = wait_cascade(ctx, queued, Status::Cancelled(CancelReason::Cascade), &p).await?;
        ensure!(
            !was_leased(&view),
            "{} was leased before it ended",
            title(&view)
        );
    }
    let stopped = wait_cascade(ctx, &r, Status::Cancelled(CancelReason::Cascade), &p).await?;
    ensure!(
        field(&stopped, "lease").is_null(),
        "the running child's lease was not revoked"
    );
    ensure!(
        kept_evidence(&stopped),
        "the running child's partial evidence was lost"
    );
    let finished = expect_status(ctx, &d1, Status::Done, "the done child keeps its status").await?;
    ensure!(
        !evidence(&finished).is_empty(),
        "the done child lost its evidence"
    );
    expect_status(
        ctx,
        &p,
        Status::Cancelled(CancelReason::Requested),
        "P is cancelled as requested",
    )
    .await?;
    Ok(())
}

/// Scenario 24: a expires: b, blocked by a, is `blocked(upstream_expired)` with
/// origin a; c, waiting for a, becomes ready; the user is told once. When
/// parent P expires its open children end `cancelled(cascade)` and nothing
/// under it runs.
async fn s24(ctx: &Ctx) -> Result<()> {
    let conversation = telegram_thread(ctx, 124).await?;
    let first = tag(24);
    let mut shelter = node(
        "a",
        "P",
        "personal",
        "Reserve the park shelter",
        &first,
        W_SUCCEED,
    );
    shelter["trigger"] = trigger_at(3600);
    shelter["expires_at"] = json!(later(4));
    let filed = file_graph(
        ctx,
        vec![
            root_node("P", "Plan the picnic", &first),
            shelter,
            node(
                "b",
                "P",
                "personal",
                "Bring the grill to the shelter",
                &first,
                W_SUCCEED,
            ),
            node(
                "c",
                "P",
                "personal",
                "Tell everyone the plan",
                &first,
                W_SUCCEED,
            ),
        ],
        vec![
            edge("b", EdgeKind::Blocks, "a"),
            edge("c", EdgeKind::WaitsFor, "a"),
        ],
        Some(&conversation),
    )
    .await?;
    let [_, a, b, c] = ids(&filed, ["P", "a", "b", "c"])?;
    let expired = wait_status(ctx, &a, Status::Expired).await?;
    let held = wait_cascade(ctx, &b, Status::Blocked(BlockedReason::UpstreamExpired), &a).await?;
    ensure!(!was_leased(&held), "b ran behind an expired upstream");
    let released = wait_for(ctx, &c, "c leaves queued once a expired", |v| {
        status(v).is_ok_and(|s| s != Status::Queued)
    })
    .await?;
    let now = status(&released)?;
    ensure!(
        now == Status::Ready || now.is_active() || now == Status::Done,
        "c became ready, since expiry is terminal: {now}"
    );
    told_once(ctx, &title(&expired)).await?;

    let second = tag(24);
    let mut picnic = root_node("P", "Plan the rained-off picnic", &second);
    picnic["expires_at"] = json!(later(4));
    let deferred = |tmp: &str, name: &str| {
        let mut item = node(tmp, "P", "personal", name, &second, W_SUCCEED);
        item["trigger"] = trigger_at(3600);
        item
    };
    let filed = file_graph(
        ctx,
        vec![
            picnic,
            deferred("x", "Buy the sandwiches"),
            deferred("y", "Pack the rug"),
        ],
        vec![],
        Some(&conversation),
    )
    .await?;
    let [p, x, y] = ids(&filed, ["P", "x", "y"])?;
    wait_status(ctx, &p, Status::Expired).await?;
    for child in [&x, &y] {
        let view = wait_cascade(ctx, child, Status::Cancelled(CancelReason::Cascade), &p).await?;
        ensure!(
            !was_leased(&view),
            "{} ran under an expired parent",
            title(&view)
        );
    }
    Ok(())
}

/// Scenario 25: The worker holding b in a, b, c, d returns drafts in which c2 and d2
/// supersede c and d, c2 blocked by b; submitted as one graph, c and d end
/// `cancelled(superseded)` and c2 runs when b is done. Superseding an item a
/// worker is running is refused whole with `supersedes_active`; a further
/// re-plan beyond the policy rate is refused with `rate_limited`.
///
/// The drafts go through `POST /api/work/plan`, the validator the
/// controller submits them to: a static script cannot name c's and d's
/// run-time ids in a `result_report`.
async fn s25(ctx: &Ctx) -> Result<()> {
    let tag = tag(25);
    let filed = file_graph(
        ctx,
        vec![
            root_node("P", "Move house", &tag),
            node("a", "P", "personal", "Pack the kitchen", &tag, W_SUCCEED),
            node("b", "P", "personal", "Pack the books", &tag, W_SLOW),
            node("c", "P", "personal", "Load the van", &tag, W_SUCCEED),
            node(
                "d",
                "P",
                "personal",
                "Unload at the new flat",
                &tag,
                W_SUCCEED,
            ),
        ],
        vec![
            edge("b", EdgeKind::Blocks, "a"),
            edge("c", EdgeKind::Blocks, "b"),
            edge("d", EdgeKind::Blocks, "c"),
        ],
        None,
    )
    .await?;
    let [p, _, b, c, d] = ids(&filed, ["P", "a", "b", "c", "d"])?;
    wait_for(ctx, &b, "b is being worked", is_active).await?;
    let replacement = |tmp: &str, name: &str| {
        let mut item = draft("personal", name, &tag, W_SUCCEED);
        item["tmp"] = json!(tmp);
        item["budget"] = budget(20_000);
        item
    };
    let replan = accepted(
        plan(
            ctx,
            json!({
                "root": p,
                "items": [replacement("c2", "Load the van in two trips"), replacement("d2", "Unload in two trips")],
                "edges": [
                    { "item": { "tmp": "c2" }, "kind": "supersedes", "depends_on": c },
                    { "item": { "tmp": "d2" }, "kind": "supersedes", "depends_on": d },
                    { "item": { "tmp": "c2" }, "kind": "blocks", "depends_on": b },
                    { "item": { "tmp": "d2" }, "kind": "blocks", "depends_on": { "tmp": "c2" } },
                ],
                "rationale": "c and d are wrong",
            }),
        )
        .await?,
    )?;
    let [c2, d2] = ids(&replan, ["c2", "d2"])?;
    for old in [&c, &d] {
        expect_status(
            ctx,
            old,
            Status::Cancelled(CancelReason::Superseded),
            "a superseded item ends cancelled(superseded)",
        )
        .await?;
    }
    let before = children(ctx, &p).await?;
    let refused = rejected(
        plan(
            ctx,
            json!({
                "root": p,
                "items": [replacement("b2", "Pack the books twice")],
                "edges": [ { "item": { "tmp": "b2" }, "kind": "supersedes", "depends_on": b } ],
                "rationale": "b is wrong",
            }),
        )
        .await?,
    )?;
    ensure!(
        reasons(&refused).contains(&RejectionReason::SupersedesActive),
        "superseding a running item is refused with supersedes_active: {:?}",
        reasons(&refused)
    );
    ensure!(
        children(ctx, &p).await? == before && is_active(&item(ctx, &b).await?),
        "the refused re-plan changed something"
    );
    let limited = rejected(
        plan(
            ctx,
            json!({
                "root": p,
                "items": [replacement("d3", "Unload tomorrow")],
                "edges": [
                    { "item": { "tmp": "d3" }, "kind": "supersedes", "depends_on": d2 },
                    { "item": { "tmp": "d3" }, "kind": "blocks", "depends_on": c2 },
                ],
                "rationale": "unload tomorrow",
            }),
        )
        .await?,
    )?;
    ensure!(
        reasons(&limited).contains(&RejectionReason::RateLimited),
        "a re-plan beyond the policy rate is refused with rate_limited: {:?}",
        reasons(&limited)
    );
    let loaded = wait_status(ctx, &c2, Status::Done).await?;
    ensure!(
        leased_at(&loaded)? >= closed_at(&item(ctx, &b).await?)?,
        "c2 ran before b was done"
    );
    Ok(())
}

fn tree_children(node: &Value) -> Vec<Value> {
    field(node, "children")
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn titled(nodes: &[Value], name: &str) -> Result<Value> {
    nodes
        .iter()
        .find(|n| title(n).contains(name))
        .cloned()
        .ok_or_else(|| {
            anyhow!(
                "no node titled {name:?} among {:?}",
                nodes.iter().map(title).collect::<Vec<_>>()
            )
        })
}

/// Scenario 26: A `code` slice imported from a recorded `StackManifest`: a parent
/// for the slice, a child parent per layer with its acceptance as
/// `done_when` and `blocks` edges between layers, a `code` child per work
/// item with `blocks` edges from `delivery_dependencies`. No `work_plan`,
/// no planner; readiness follows the delivery DAG; a cyclic manifest is
/// rejected whole with `cycle`.
async fn s26(ctx: &Ctx) -> Result<()> {
    let tag = tag(26);
    let slice = format!("Work items over REST {tag}");
    let imported = accepted(
        filing(
            ctx,
            WORK_IMPORT,
            json!({ "manifest": stack_manifest(&slice, false) }),
        )
        .await?,
    )?;
    let tree = graph_tree(ctx, &imported.root).await?;
    ensure!(
        title(&tree) == slice,
        "one parent item for the slice: {}",
        title(&tree)
    );
    let layers = tree_children(&tree);
    ensure!(
        layers.len() == 2,
        "one child parent per stack layer, got {}",
        layers.len()
    );
    let (lower, upper) = (
        titled(&layers, "Persist work items")?,
        titled(&layers, "List work items over REST")?,
    );
    ensure!(
        body(&lower)["done_when"] == "work items survive a daemon restart"
            && body(&upper)["done_when"] == "GET /api/work lists open items",
        "each layer's acceptance is its done_when"
    );
    ensure!(
        has_edge(&upper, EdgeKind::Blocks, &id(&lower)),
        "the upper layer is blocked by the one below: {:?}",
        edges(&upper)
    );
    let (below, above) = (tree_children(&lower), tree_children(&upper));
    ensure!(
        below.len() == 2
            && above.len() == 1
            && below
                .iter()
                .chain(&above)
                .all(|n| body(n)["kind"] == "code"),
        "a code child per delivery work item, under its layer"
    );
    let (table, store_api, route) = (
        titled(&below, "Add the work_items table")?,
        titled(&below, "Add the store API")?,
        titled(&above, "Add the list route")?,
    );
    ensure!(
        has_edge(&store_api, EdgeKind::Blocks, &id(&table)),
        "delivery_dependencies became blocks edges: {:?}",
        edges(&store_api)
    );
    let planned = strings(
        ctx,
        "SELECT conversation_id FROM messages WHERE instr(data, 'work_plan') > 0 AND instr(data, ?1) > 0",
        &slice,
    )?;
    ensure!(
        planned.is_empty(),
        "a work_plan call or the planner touched the slice: {planned:?}"
    );
    let first = status(&item(ctx, &id(&table)).await?)?;
    ensure!(
        first != Status::Queued,
        "the first delivery item is not ready: {first}"
    );
    if first != Status::Done {
        for waiting in [&store_api, &route] {
            let now = status(&item(ctx, &id(waiting)).await?)?;
            ensure!(
                now == Status::Queued,
                "{} is {now} ahead of the delivery order",
                title(waiting)
            );
        }
    }
    let cyclic = format!("Cyclic slice {tag}");
    let refused = rejected(
        filing(
            ctx,
            WORK_IMPORT,
            json!({ "manifest": stack_manifest(&cyclic, true) }),
        )
        .await?,
    )?;
    ensure!(
        reasons(&refused).contains(&RejectionReason::Cycle),
        "a manifest with a cycle is rejected with cycle: {:?}",
        reasons(&refused)
    );
    ensure!(
        find_titled(ctx, &cyclic).await?.is_none(),
        "something from the cyclic manifest was stored"
    );
    Ok(())
}

/// Scenario 28: With 500 closed items seeded, those past the policy window are
/// compacted to one-line summaries; `work list` returns only open and recent
/// items; `work archive search` finds a compacted item by its summary; no
/// open item is compacted, nor a closed item an open item names in
/// `inputs_from`; an open item's `discovered_from` link to a compacted item
/// resolves to its summary.
async fn s28(ctx: &Ctx) -> Result<()> {
    let tag = tag(28);
    let closed = seed_closed_items(ctx, &tag, 500).await?;
    let (named_input, compacted) = (closed[0].clone(), closed[1].clone());
    let open_item = |name: &str| {
        let mut item = draft("personal", name, &tag, W_SUCCEED);
        item["trigger"] = not_yet();
        item
    };
    let open = file(ctx, open_item("Plan the next errand")).await?;
    let mut reader = open_item("Summarise last month's errands");
    reader["edges"] = json!([{ "kind": "waits_for", "depends_on": named_input }]);
    reader["inputs_from"] = json!([named_input]);
    let reader = file(ctx, reader).await?;
    let mut follow_up = open_item("Follow up on errand 1");
    follow_up["edges"] = json!([{ "kind": "discovered_from", "depends_on": compacted }]);
    let follow_up = file(ctx, follow_up).await?;
    backdate(ctx, &closed[..400], 90)?;

    let search = query(&[("search", tag.as_str())])?;
    let deadline = Instant::now() + SETTLE;
    let archived = loop {
        let answer = get(ctx, &format!("{WORK_ARCHIVE}?{search}")).await?;
        let rows = archive_rows(&answer);
        if rows.iter().any(|r| r["id"] == compacted.as_str()) {
            break rows;
        }
        ensure!(
            Instant::now() < deadline,
            "nothing was compacted after {SETTLE:?}"
        );
        tokio::time::sleep(POLL).await;
    };
    let archived_ids: Vec<&str> = archived.iter().filter_map(|r| r["id"].as_str()).collect();
    let missing = closed[1..400]
        .iter()
        .filter(|i| !archived_ids.contains(&i.as_str()))
        .count();
    ensure!(
        missing == 0,
        "{missing} closed items past the window were not compacted"
    );
    ensure!(
        archived.iter().all(|r| r["summary"]
            .as_str()
            .is_some_and(|s| !s.is_empty() && !s.contains('\n'))),
        "a compacted item is not a one-line summary"
    );
    ensure!(
        closed[400..]
            .iter()
            .all(|i| !archived_ids.contains(&i.as_str())),
        "a closed item inside the window was compacted"
    );
    ensure!(
        !archived_ids.contains(&named_input.as_str()),
        "a closed item an open item names in inputs_from was compacted"
    );
    ensure!(
        [&open, &reader, &follow_up]
            .iter()
            .all(|i| !archived_ids.contains(&i.as_str())),
        "an open item was compacted"
    );
    let listing = cli_stdout(ctx, &["work", "list"])?;
    ensure!(
        listing.contains(&format!("Plan the next errand {tag}")),
        "work list leaves out an open item"
    );
    ensure!(
        !(1..400).any(|n| listing.contains(&format!("Closed errand {n} {tag}"))),
        "work list includes a compacted item"
    );
    let summary = archived
        .iter()
        .find(|r| r["id"] == compacted.as_str())
        .and_then(|r| r["summary"].as_str())
        .unwrap_or_default()
        .to_string();
    let found = cli_stdout(ctx, &["work", "archive", "search", &summary])?;
    ensure!(
        found.contains(&compacted) || found.contains(&summary),
        "work archive search did not find the compacted item by its summary: {found}"
    );
    let escaped = Value::String(summary.clone()).to_string();
    let escaped = escaped.trim_matches('"');
    ensure!(
        item(ctx, &follow_up).await?.to_string().contains(escaped),
        "the open item's discovered_from link does not resolve to the archive line"
    );
    Ok(())
}

/// Scenario 30: A scheduled job fires while an interactive turn runs on the same
/// model: the firing is an item with `trigger: at(time)` that runs after the
/// turn. After a restart two overdue jobs run one at a time; both appear in
/// `work list`; no cron path starts a conversation outside the controller.
async fn s30(ctx: &Ctx) -> Result<()> {
    let tag = tag(30);
    let task = |what: &str| format!("{W_SUCCEED}. {what} {tag}");
    let job = open_store(ctx)?
        .jobs()
        .create_job(
            &later(2),
            &task("Water the plants"),
            None,
            None,
            None,
            "UTC",
            true,
        )
        .await?;
    let conversation = ctx.create_conversation().await?;
    let started = Instant::now();
    ctx.send(&conversation, S30_TURN).await?;
    let turn_ended = Utc::now();
    ensure!(
        started.elapsed() >= Duration::from_secs(5),
        "the interactive turn was too short to overlap the job"
    );
    let firing = wait_job_item(ctx, &job.id).await?;
    let fired = wait_status(ctx, &firing, Status::Done).await?;
    ensure!(
        body(&fired)["trigger"]["kind"] == "at",
        "the firing is an item with trigger at(time): {}",
        body(&fired)["trigger"]
    );
    ensure!(
        leased_at(&fired)? + chrono::Duration::seconds(1) >= turn_ended,
        "the firing ran during the interactive turn"
    );

    let overdue = Mutex::new(Vec::new());
    ctx.restart_daemon_with(|| async {
        let store = open_store(ctx)?;
        for what in ["Feed the cat", "Take out the bins"] {
            let job = store
                .jobs()
                .create_job(&later(1), &task(what), None, None, None, "UTC", true)
                .await?;
            overdue.lock().unwrap().push(job.id);
        }
        // Both fall due while nothing is running.
        tokio::time::sleep(Duration::from_secs(2)).await;
        Ok(())
    })
    .await?;
    let jobs = overdue.lock().unwrap().clone();
    let mut firings = Vec::new();
    let mut spans = Vec::new();
    for job in &jobs {
        let firing = wait_job_item(ctx, job).await?;
        spans.push(active_span(
            &wait_status(ctx, &firing, Status::Done).await?,
        )?);
        firings.push(firing);
    }
    ensure!(
        spans.len() == 2 && (spans[0].1 <= spans[1].0 || spans[1].1 <= spans[0].0),
        "the overdue jobs ran together: {spans:?}"
    );
    // Done by now, so listed with the closed items (`work list --all`):
    // plain `work list` is open items only (plan 14.2).
    let listed: Vec<String> = list(ctx, "include_closed=true")
        .await?
        .iter()
        .map(id)
        .collect();
    for firing in &firings {
        ensure!(
            listed.contains(firing),
            "work list leaves out the job's item {firing}"
        );
    }
    for what in ["Water the plants", "Feed the cat", "Take out the bins"] {
        let runs = conversations_mentioning(ctx, &format!("{what} {tag}"))?;
        ensure!(
            runs.len() == 1,
            "{what:?} ran in {} conversations, want only the controller's worker run",
            runs.len()
        );
    }
    Ok(())
}

/// Scenario 32: The daemon restarts mid-graph, with one child leased, one held
/// behind a failed sibling and the parent's message pending: readiness,
/// cascade and roll-up re-derive to the same state, nothing is re-planned,
/// the leased child resumes or returns to ready, and the parent's message
/// is sent exactly once.
async fn s32(ctx: &Ctx) -> Result<()> {
    let tag = tag(32);
    let conversation = telegram_thread(ctx, 132).await?;
    let filed = file_graph(
        ctx,
        vec![
            root_node("P", "Prepare the garden party", &tag),
            node("f", "P", "personal", "Hire the marquee", &tag, W_TIMEOUT),
            node("h", "P", "personal", "Put up the marquee", &tag, W_SUCCEED),
            node("l", "P", "personal", "Mow the lawn", &tag, W_SLOW),
        ],
        vec![
            edge("h", EdgeKind::Blocks, "f"),
            edge("l", EdgeKind::WaitsFor, "f"),
        ],
        Some(&conversation),
    )
    .await?;
    let [p, f, h, l] = ids(&filed, ["P", "f", "h", "l"])?;
    wait_status(ctx, &f, Status::Failed).await?;
    wait_cascade(ctx, &h, Status::Blocked(BlockedReason::UpstreamFailed), &f).await?;
    wait_for(ctx, &l, "l is leased", is_active).await?;
    let before = snapshot(ctx, &[&p, &f, &h]).await?;
    let children_before = children(ctx, &p).await?;
    ctx.restart_daemon().await?;
    let after = snapshot(ctx, &[&p, &f, &h]).await?;
    ensure!(
        after == before,
        "readiness, cascade or roll-up changed across the restart: {before:?} then {after:?}"
    );
    let now = status(&item(ctx, &l).await?)?;
    ensure!(
        now == Status::Ready || now.is_active() || now == Status::Done,
        "the leased child neither resumed nor returned to ready: {now}"
    );
    let mowed = wait_status(ctx, &l, Status::Done).await?;
    ensure!(
        !events(&mowed)
            .iter()
            .any(|e| moved_to(e) == Some(Status::Failed)),
        "the restart failed the leased child"
    );
    ensure!(
        children(ctx, &p).await? == children_before,
        "the restart re-planned the graph"
    );
    told_once(ctx, &tag).await?;
    Ok(())
}

// ── Phase 3: the worker registry and the first external worker ───────

/// Scenario 2: A coding task runs on a `claude_code` worker in a worktree; the
/// controller verifies the commit and changed paths; a result whose claimed
/// paths differ from the diff is marked `verification_failed`.
async fn s02(ctx: &Ctx) -> Result<()> {
    // Verify this adapter's Git claims independently of model routing.
    // Generic-pool coding and escalation are covered by scenario 17.
    let daemon = OwnDaemon::boot(ctx).await?;
    let ctx = &daemon.ctx;
    let tag = tag(2);
    let repo = FixtureRepo::create()?;
    let stand_in = ClaudeCodeStandIn::create()?;
    register_claude_code(ctx, "pinch", &repo, &stand_in).await?;
    let mut change = code_draft("Add a status helper", &tag, "Implement the change", &repo);
    change["worker_kind"] = json!("claude_code");
    let task = file(ctx, change).await?;
    let finished = wait_status(ctx, &task, Status::Done).await?;
    ensure!(
        worker_of(&finished).as_deref() == Some("pinch"),
        "the task ran on {:?}, not pinch",
        worker_of(&finished)
    );
    let commit = evidence(&finished)
        .into_iter()
        .find(|e| e["kind"] == "commit")
        .ok_or_else(|| anyhow!("no commit evidence: {}", evidence_text(&finished)))?;
    let sha = commit["reference"].as_str().unwrap_or_default();
    ensure!(
        commit["verified_by"].is_string()
            && repo.git(&["cat-file", "-t", sha]).ok().as_deref() == Some("commit"),
        "the controller verified a commit that exists in the repository: {commit}"
    );
    ensure!(
        evidence(&finished)
            .iter()
            .any(|e| e["reference"] == "src/lib.rs" && e["verified_by"].is_string()),
        "the changed path was verified against the diff: {}",
        evidence_text(&finished)
    );
    let mut false_claim = code_draft("Add a second status helper", &tag, CLAIM_WRONGLY, &repo);
    false_claim["worker_kind"] = json!("claude_code");
    let claimed = file(ctx, false_claim).await?;
    wait_status(
        ctx,
        &claimed,
        Status::Blocked(BlockedReason::VerificationFailed),
    )
    .await?;
    Ok(())
}

/// Each worker's routing record, by name.
fn routing_records(workers: &[Value]) -> Vec<(String, Value)> {
    workers
        .iter()
        .map(|w| {
            (
                w["name"].as_str().unwrap_or_default().to_string(),
                w["routing_record"].clone(),
            )
        })
        .collect()
}

fn verified_done(workers: &[Value], name: &str) -> u64 {
    workers
        .iter()
        .filter(|w| w["name"] == name)
        .flat_map(|w| {
            w["routing_record"]
                .as_object()
                .map(|classes| classes.values().cloned().collect::<Vec<_>>())
                .unwrap_or_default()
        })
        .filter_map(|class| class["verified_done"].as_u64())
        .sum()
}

/// Scenario 17: A `code` item on a local worker is verified and done, and the
/// worker's routing record for its class improves. A second item of the
/// class fails verification twice locally, escalates to `claude_code` and
/// passes; dreaming files a routing proposal citing both records, and the
/// controller does not move the class's default on its own.
async fn s17(ctx: &Ctx) -> Result<()> {
    let tag = tag(17);
    let repo = FixtureRepo::create()?;
    let stand_in = ClaudeCodeStandIn::create()?;
    let first = file(
        ctx,
        code_draft("Document the status helper", &tag, W_CODE_LOCAL, &repo),
    )
    .await?;
    let before = workers(ctx).await?;
    let documented = wait_status(ctx, &first, Status::Done).await?;
    let local = worker_of(&documented).unwrap_or_default();
    ensure!(
        before
            .iter()
            .any(|w| w["name"] == local.as_str() && w["kind"] == "local"),
        "the first code item ran on {local:?}, not a local worker"
    );
    let after = workers(ctx).await?;
    ensure!(
        verified_done(&after, &local) > verified_done(&before, &local),
        "the local worker's routing record for the class did not improve"
    );
    let second = file(
        ctx,
        code_draft("Document the helper's errors", &tag, W_CODE_BAD, &repo),
    )
    .await?;
    // Make the fallback available after the two local attempts have started.
    // Unrelated busy work must not make this fixture depend on a backend pin.
    wait_for(ctx, &second, "two local verification attempts", |v| {
        events(v)
            .iter()
            .filter(|e| moved_to(e) == Some(Status::Leased))
            .count()
            >= 2
    })
    .await?;
    register_claude_code(ctx, "pinch-s17", &repo, &stand_in).await?;
    let escalated = wait_status(ctx, &second, Status::Done).await?;
    let failed_checks = events(&escalated)
        .iter()
        .filter(|e| moved_to(e) == Some(Status::Blocked(BlockedReason::VerificationFailed)))
        .count();
    ensure!(
        failed_checks >= 2,
        "it failed verification {failed_checks} times locally, want twice before escalating"
    );
    ensure!(
        worker_of(&escalated).as_deref() == Some("pinch-s17"),
        "it passed on {:?}, not the claude_code worker",
        worker_of(&escalated)
    );
    let records = routing_records(&workers(ctx).await?);
    evaluate(ctx).await?;
    let routing = proposals_mentioning(ctx, &first).await?;
    ensure!(
        routing.iter().any(|p| p.to_string().contains(&second)),
        "no routing proposal cites both records"
    );
    ensure!(
        routing_records(&workers(ctx).await?) == records,
        "the controller moved the routing default on its own"
    );
    Ok(())
}

/// Scenario 31: A `claude_code` worker that keeps its own task list and a Beads
/// database in its worktree returns one `discovered` draft: exactly one item
/// is filed, with `discovered_from` set, and nothing from the worker's own
/// tracker reaches the store.
async fn s31(ctx: &Ctx) -> Result<()> {
    // Isolate the native adapter's private tracker contract. General routing
    // is exercised separately; the scripted model has no private Beads ledger.
    let daemon = OwnDaemon::boot(ctx).await?;
    let ctx = &daemon.ctx;
    let tag = tag(31);
    let repo = FixtureRepo::create()?;
    let stand_in = ClaudeCodeStandIn::create()?;
    register_claude_code(ctx, "pinch-s31", &repo, &stand_in).await?;
    let change = code_draft(
        "Add a status helper and track it",
        &tag,
        KEEPS_ITS_OWN_TRACKER,
        &repo,
    );
    let task = file(ctx, change).await?;
    wait_status(ctx, &task, Status::Done).await?;
    let discovered_title = "Add a changelog entry [e2e-control s31]";
    let discovered = wait_titled(ctx, discovered_title).await?;
    tokio::time::sleep(QUIET).await;
    // Closed items too: the follow-up inherits its parent's worker and
    // repository, so it may already have run by the time this counts.
    let everything = list(ctx, "include_closed=true").await?;
    ensure!(
        everything
            .iter()
            .filter(|v| title(v) == discovered_title)
            .count()
            == 1,
        "the one draft was not filed exactly once"
    );
    ensure!(
        has_edge(&discovered, EdgeKind::DiscoveredFrom, &task),
        "the filed draft has no discovered_from edge on the worker's item: {:?}",
        edges(&discovered)
    );
    ensure!(
        !everything
            .iter()
            .any(|v| title(v).contains("beads task") || title(v).contains("claude task")),
        "something from the worker's own tracker reached the store"
    );
    Ok(())
}

/// A scripted daemon of its own, killed when the scenario ends however it
/// ends: with `RUSTYKRAB_LOCAL_WORKER=off` ([`OwnDaemon::boot`]), or with
/// its local worker and a fresh store ([`OwnDaemon::boot_with`]).
struct OwnDaemon {
    ctx: Ctx,
    _dir: tempfile::TempDir,
}

impl Drop for OwnDaemon {
    fn drop(&mut self) {
        if let Ok(mut daemon) = self.ctx.daemon.try_lock() {
            if let Some(mut child) = daemon.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

impl OwnDaemon {
    async fn boot(shared: &Ctx) -> Result<OwnDaemon> {
        OwnDaemon::boot_with(shared, &[("RUSTYKRAB_LOCAL_WORKER", "off")]).await
    }

    /// A daemon with `env` on top of the harness's own environment. A
    /// value may name the daemon's data directory as `__E2E_DATA_DIR__`.
    async fn boot_with(shared: &Ctx, env: &[(&str, &str)]) -> Result<OwnDaemon> {
        let dir = tempfile::Builder::new()
            .prefix("rustykrab-e2e-own-")
            .tempdir()?;
        let port = crate::pick_free_port()?;
        let base = format!("http://127.0.0.1:{port}");
        let data_dir = dir.path().to_string_lossy().to_string();
        let bound: Vec<(&str, String)> = env
            .iter()
            .map(|(k, v)| (*k, v.replace("__E2E_DATA_DIR__", &data_dir)))
            .collect();
        let env: Vec<(&str, &str)> = bound.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let mut child = crate::spawn_daemon_env(
            &shared.bin,
            dir.path(),
            port,
            &crate::Backend::Scripted(None),
            &env,
        )?;
        if let Err(e) = crate::wait_for_health(&base, &shared.client, &mut child).await {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
        let store = rustykrab_store::Store::open(
            dir.path().join("db"),
            crate::hex_decode(crate::MASTER_KEY_HEX)?,
        )?;
        let ctx = Ctx {
            base,
            client: shared.client.clone(),
            secrets: store.secrets(),
            db_path: dir.path().join("db").join("store.db"),
            bin: shared.bin.clone(),
            data_dir: dir.path().to_path_buf(),
            daemon: std::sync::Arc::new(tokio::sync::Mutex::new(Some(child))),
            stand_ins: None,
        };
        Ok(OwnDaemon { ctx, _dir: dir })
    }
}

/// A daemon with the local worker off and one external worker: an item
/// that names no worker kind, which routing would otherwise give the
/// cheapest-tier local worker, is leased to the external worker, and
/// `GET /api/workers` lists no live local worker. It boots its own daemon,
/// since the shared one's local worker is what the other scenarios run on.
async fn local_worker_off(shared: &Ctx) -> Result<()> {
    let daemon = OwnDaemon::boot(shared).await?;
    let ctx = &daemon.ctx;
    let before = workers(ctx).await?;
    ensure!(
        !before
            .iter()
            .any(|w| w["kind"] == "local" && w["live"] == true),
        "a live local worker is registered with the local worker off: {before:?}"
    );
    let repo = FixtureRepo::create()?;
    let stand_in = ClaudeCodeStandIn::create()?;
    register_claude_code(ctx, "pinch-local-off", &repo, &stand_in).await?;
    let change = code_draft(
        "Add a status helper",
        &tag(0),
        "Implement the change",
        &repo,
    );
    ensure!(
        change.get("worker_kind").is_none(),
        "the draft must leave the worker unconstrained: {change}"
    );
    let task = file(ctx, change).await?;
    let finished = wait_status(ctx, &task, Status::Done).await?;
    ensure!(
        worker_of(&finished).as_deref() == Some("pinch-local-off"),
        "the unconstrained item ran on {:?}, not the external worker",
        worker_of(&finished)
    );
    let after = workers(ctx).await?;
    ensure!(
        after
            .iter()
            .any(|w| w["name"] == "pinch-local-off" && w["live"] == true),
        "the external worker is not live: {after:?}"
    );
    ensure!(
        !after
            .iter()
            .any(|w| w["kind"] == "local" && w["live"] == true),
        "GET /api/workers lists a live local worker: {after:?}"
    );
    Ok(())
}

// ── the investigation kit ────────────────────────────────────────────

/// Where a run of the suite leaves what it captured, when the caller asks
/// for it (`RUSTYKRAB_E2E_EVIDENCE_DIR`): the outputs a reviewer reads,
/// copied as the daemon and its worker produced them.
fn evidence_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("RUSTYKRAB_E2E_EVIDENCE_DIR")
        .map(std::path::PathBuf::from)
        .filter(|dir| !dir.as_os_str().is_empty())
}

/// Save one captured output under the evidence directory, if there is one.
fn keep_evidence(name: &str, contents: impl AsRef<[u8]>) -> Result<()> {
    let Some(dir) = evidence_dir() else {
        return Ok(());
    };
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))
}

/// Copy one file a run left behind under the evidence directory, if there
/// is one, as it stands.
fn keep_evidence_file(name: &str, from: &std::path::Path) -> Result<()> {
    if evidence_dir().is_none() {
        return Ok(());
    }
    let contents = std::fs::read(from).with_context(|| format!("reading {}", from.display()))?;
    keep_evidence(name, contents)
}

/// A local worker's run leaves its whole session as a file, and the item's
/// `run_log` evidence names it: a marker line, then one JSON message per
/// line, readable by its owner only.
async fn local_worker_session_log(ctx: &Ctx) -> Result<()> {
    let task = file(
        ctx,
        draft("personal", "Leave a session log", &tag(0), W_SUCCEED),
    )
    .await?;
    let done = wait_status(ctx, &task, Status::Done).await?;
    let log = evidence(&done)
        .into_iter()
        .find(|e| e["kind"] == "run_log")
        .ok_or_else(|| anyhow!("no run_log evidence: {}", evidence_text(&done)))?;
    let path = std::path::PathBuf::from(log["reference"].as_str().unwrap_or_default());
    ensure!(
        path.starts_with(ctx.data_dir.join("run-logs")),
        "the session log is not under <data dir>/run-logs: {}",
        path.display()
    );
    let run = log["hash"].as_str().unwrap_or_default();
    ensure!(
        path.file_name().and_then(|n| n.to_str()) == Some(&format!("{run}.jsonl")),
        "the session log is not named after its run {run}: {}",
        path.display()
    );
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading the session log {}", path.display()))?;
    let lines: Vec<Value> = text
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()
        .context("every line of the session log is one JSON value")?;
    ensure!(
        lines[0]["type"] == "rustykrab.invocation"
            && lines[0]["kind"] == "local"
            && lines[0]["run"] == run,
        "the session log starts with its marker: {}",
        lines[0]
    );
    ensure!(
        lines[0]["messages"] == lines.len() - 1 && lines.len() > 3,
        "the session log holds the whole run: {} line(s)",
        lines.len()
    );
    ensure!(
        text.contains(W_SUCCEED) && text.contains("result_report"),
        "the session log carries the brief and the report call"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path)?.permissions().mode() & 0o777;
        ensure!(mode == 0o600, "the session log is mode {mode:o}, not 600");
    }
    keep_evidence("local-worker-run-log.jsonl", &text)?;
    keep_evidence(
        "local-worker-run-log.evidence.json",
        serde_json::to_string_pretty(&log)?,
    )?;
    Ok(())
}

/// A fake token in the shape of a real one, for the daemon log fixture.
const FAKE_BOT_TOKEN: &str = "7000000001:AAFakeFakeFakeFakeFakeFakeFakeFake_0";

/// The item's events, as `GET /api/work/{id}/events` returns them.
async fn event_trail(ctx: &Ctx, id: &str) -> Result<Vec<Value>> {
    let reply = get(ctx, &format!("{WORK}/{id}/events")).await?;
    reply["events"]
        .as_array()
        .cloned()
        .ok_or_else(|| anyhow!("GET {WORK}/{id}/events has no events: {reply}"))
}

/// One event as a line a reader can follow: when, what kind, the status
/// it moved to, who, and why.
fn trail_line(event: &Value) -> String {
    let to = match moved_to(event) {
        Some(status) => format!(" -> {status}"),
        None => String::new(),
    };
    let reason = event["reason"].as_str().unwrap_or_default();
    // A rung's reason is its record as JSON: show the rung and its outcome.
    let reason = match serde_json::from_str::<Value>(reason) {
        Ok(rung) if rung["rung"].is_string() => format!(
            "rung {}: {}",
            rung["rung"].as_str().unwrap_or_default(),
            rung["outcome"].as_str().unwrap_or_default()
        ),
        _ => reason.to_string(),
    };
    format!(
        "{}  {}{to}  [{}]  {}",
        event["at"].as_str().unwrap_or_default(),
        event["kind"].as_str().unwrap_or_default(),
        event["actor"].as_str().unwrap_or_default(),
        excerpt(&reason),
    )
}

fn trail_text(events: &[Value]) -> String {
    events.iter().map(trail_line).collect::<Vec<_>>().join("\n") + "\n"
}

/// The fix item filed for a failure of `failing`, other than `not`.
async fn wait_fix_item(ctx: &Ctx, failing: &str, not: Option<&str>) -> Result<String> {
    let found = wait_listed(ctx, "kind=internal", "a fix item for the failure", |v| {
        v.to_string().contains(failing) && not.is_none_or(|other| id(v) != other)
    })
    .await?;
    Ok(id(&found))
}

/// The investigation kit, end to end, on a daemon of its own whose only
/// worker is an external agent CLI stand-in.
///
/// Three items in one repository fail the same way (the worker claims a
/// path its commit did not touch, which the controller's own verification
/// catches), so the third files the failure's fix item. The stand-in, as
/// the investigator, records what it was actually given, and the scenario
/// asserts on that and on how the fix item closes:
///
/// - it ran in a worktree of the failing item's repository at the commit
///   the failing run started from;
/// - its brief names the investigation bundle and how to read it in
///   portions, and its CLI was given the bundle and the session logs with
///   `--add-dir` (a failed run's kept worktree too, when one is kept; these
///   runs handed in a result, so theirs were removed and their commits stay
///   on their branches);
/// - the bundle holds all three occurrences run by run, the same records
///   `rustykrab work investigate` prints, and the daemon's log for a failed
///   run's window with the bot token redacted;
/// - it reported that nothing changed, so the fix item is not `done`: it
///   parks with that conclusion as its question, and the item that waited
///   on it goes on with its own ladder;
/// - answered, it lands a commit git verifies and closes `done`;
/// - the failure then recurs, and the second fix item's brief names the
///   first, what it concluded, the commit it landed and the run that
///   failed since, with whether that run's code had the fix in it.
async fn investigation_kit(shared: &Ctx) -> Result<()> {
    let daemon = OwnDaemon::boot(shared).await?;
    let ctx = &daemon.ctx;
    let repo = FixtureRepo::create()?;
    let stand_in = ClaudeCodeStandIn::investigator()?;
    register_claude_code(ctx, "pinch-inv", &repo, &stand_in).await?;
    let tag = "[e2e-control investigation]";
    let failing = |name: &str| code_draft(name, tag, INVESTIGATION_CLAIM_WRONGLY, &repo);

    // Two items fail the same way and surface once their ladders are spent.
    let first = file(ctx, failing("Add the first helper")).await?;
    wait_question(ctx, &first).await?;
    let second = file(ctx, failing("Add the second helper")).await?;
    wait_question(ctx, &second).await?;
    let error = wait_error(ctx, &first).await?;
    ensure!(
        error.class == ErrorClass::Verification,
        "the seeded failure is the controller's own verification, not {}",
        error.class.as_str()
    );
    let fingerprint = error.fingerprint.clone();

    // A build before the Telegram fix logged the bot token on every failed
    // poll. Put such a line in the daemon's log, stamped inside the first
    // item's first failed run.
    let before: Value = get(ctx, &format!("{WORK}/investigate/{first}")).await?;
    let failed_run = &before["occurrences"][0]["runs"][0];
    let started = time(&failed_run["started_at"])
        .ok_or_else(|| anyhow!("the first failed run has no start: {failed_run}"))?;
    let leaked = format!(
        "{} ERROR rustykrab_channels::telegram: Telegram getUpdates network error: error sending \
         request for url (https://api.telegram.org/bot{FAKE_BOT_TOKEN}/getUpdates?timeout=30) \
         consecutive_errors=1 delay_secs=2\n",
        (started + chrono::TimeDelta::milliseconds(1))
            .to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
    );
    {
        use std::io::Write as _;
        let log = ctx
            .data_dir
            .join("logs")
            .join(format!("rustykrab.log.{}", started.format("%Y-%m-%d")));
        std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&log)?
            .write_all(leaked.as_bytes())?;
    }

    // The third item's failure is the recurrence that files the fix item.
    let third = file(ctx, failing("Add the third helper")).await?;
    let fix = wait_fix_item(ctx, &third, None).await?;
    let parked = wait_status(ctx, &fix, Status::Blocked(BlockedReason::NeedsDecision)).await?;

    // ── what the investigator was given ──────────────────────────────
    let captures = ctx.data_dir.join(INVESTIGATOR_CAPTURES);
    let cap = captures.join("run-1");
    let read = |name: &str| -> Result<String> {
        std::fs::read_to_string(cap.join(name))
            .with_context(|| format!("the investigator left no {name}"))
    };
    let bundle: Value = serde_json::from_str(&read("bundle/bundle.json")?)?;
    let history = &bundle["history"];
    let occurrences = history["occurrences"]
        .as_array()
        .context("the bundle has occurrences")?;
    let ids: Vec<&str> = occurrences
        .iter()
        .filter_map(|o| o["item"]["id"].as_str())
        .collect();
    ensure!(
        ids == [first.as_str(), second.as_str(), third.as_str()],
        "the bundle holds every item that hit the failure: {ids:?}"
    );
    ensure!(
        bundle["bundle"]["fingerprint"] == fingerprint.as_str()
            && bundle["bundle"]["for_item"] == fix.as_str(),
        "the bundle is for this fix item and fingerprint: {}",
        bundle["bundle"]
    );
    // Each failed run carries its worker, its typed error, its workspace
    // and its session log.
    let third_run = &occurrences[2]["runs"][0];
    ensure!(
        third_run["hit"] == true
            && third_run["worker"] == "pinch-inv"
            && third_run["rungs"][0]["error"]["fingerprint"] == fingerprint.as_str()
            && third_run["run_log"]["exists"] == true,
        "a failed run's record is complete: {third_run}"
    );
    // The code: a checkout of the failing item's repository at the commit
    // the failing run started from.
    let head = read("head.txt")?;
    let base = third_run["workspace"]["base"].as_str().unwrap_or_default();
    ensure!(
        !base.is_empty() && head.trim() == base,
        "the investigator's worktree is at {}, not the failing run's base {base}",
        head.trim()
    );
    let listing = read("working-directory.txt")?;
    ensure!(
        listing.contains("lib.rs") && listing.contains("Cargo.toml"),
        "the investigator's working directory is a checkout, not an empty directory: {listing}"
    );
    ensure!(
        listing.contains(&ctx.data_dir.join("worktrees").display().to_string()),
        "the investigator ran in a worktree under the daemon's data directory: {listing}"
    );
    // The brief: where the bundle is and how to read it in portions.
    let brief = read("brief.txt")?;
    let bundle_dir = bundle["bundle"]["dir"].as_str().unwrap_or_default();
    for wanted in [
        format!("investigation: this item investigates failure {fingerprint}"),
        format!("{bundle_dir}/index.md"),
        format!("{bundle_dir}/bundle.json"),
        "a portion at a time".to_string(),
        "readable outside your working directory".to_string(),
        "This item is done only when it lands something the controller can check".to_string(),
    ] {
        ensure!(
            brief.contains(&wanted),
            "the brief lacks {wanted:?}: {brief}"
        );
    }
    // Read access outside the worktree: the bundle, the session logs and a
    // failed run's kept worktree, and nothing gated.
    let args = read("args.txt")?;
    let lines: Vec<&str> = args.lines().collect();
    let added: Vec<&str> = lines
        .windows(2)
        .filter(|pair| pair[0] == "--add-dir")
        .map(|pair| pair[1])
        .collect();
    for wanted in [
        bundle_dir.to_string(),
        ctx.data_dir.join("run-logs").display().to_string(),
    ] {
        ensure!(
            added.contains(&wanted.as_str()),
            "the investigator's CLI was not given {wanted} with --add-dir: {added:?}"
        );
    }
    // These runs handed in a result, so the adapter removed their
    // worktrees: the record says so, names the branch their commit is
    // still on, and no directory that is gone is offered.
    ensure!(
        third_run["workspace"]["worktree_exists"] == false
            && third_run["workspace"]["branch"]
                .as_str()
                .is_some_and(|b| repo.git(&["rev-parse", "--verify", b]).is_ok()),
        "a failed run's commit stays on its branch: {}",
        third_run["workspace"]
    );
    ensure!(
        !added.iter().any(|dir| !std::path::Path::new(dir).exists()),
        "the investigator was offered a directory that does not exist: {added:?}"
    );
    ensure!(
        added.len() == 2 && !args.contains("--tools"),
        "nothing else was added and its toolset is untouched: {args}"
    );
    ensure!(
        read("read-index.txt")?.contains(&format!("failure `{fingerprint}`"))
            && read("read-session-log.txt")?.contains("\"type\":\"result\""),
        "the investigator read the index and a failed run's session log"
    );
    // The daemon log for the failed run's window, with the token gone.
    let slice_path = occurrences[0]["runs"][0]["daemon_log"]["path"]
        .as_str()
        .context("the first failed run has a daemon log slice")?;
    let slice = std::fs::read_to_string(
        cap.join("bundle/daemon-log").join(
            std::path::Path::new(slice_path)
                .file_name()
                .context("the slice has a file name")?,
        ),
    )?;
    ensure!(
        slice.contains("https://api.telegram.org/bot<redacted>/getUpdates?timeout=30"),
        "the slice holds the fixture line, redacted: {slice}"
    );
    ensure!(
        !slice.contains("AAFake") && !read("bundle/bundle.json")?.contains("AAFake"),
        "the bot token reached the bundle"
    );

    // ── the same history for an operator ─────────────────────────────
    let printed = cli_stdout(ctx, &["work", "investigate", &fingerprint, "--json"])?;
    let investigated: Value = serde_json::from_str(&printed)?;
    ensure!(
        investigated["fingerprints"][0]["fingerprint"] == fingerprint.as_str()
            && investigated["fingerprints"][0]["items"] == 3
            && investigated["occurrences"].as_array().map(Vec::len) == Some(3),
        "work investigate prints every occurrence: {}",
        investigated["fingerprints"]
    );
    let record = &investigated["fixes"][0];
    ensure!(
        record["item"]["id"] == fix.as_str()
            && record["landed"].as_array().is_some_and(Vec::is_empty)
            && record["conclusion"]
                .as_str()
                .is_some_and(|c| c.starts_with(INVESTIGATION_NOT_DONE)),
        "the fix item's record says what it concluded and that it landed nothing: {record}"
    );
    let text = cli_stdout(ctx, &["work", "investigate", &fingerprint])?;
    ensure!(
        text.contains(&format!(
            "failure {fingerprint}  verification/claim_mismatch"
        )) && text.contains("landed nothing the controller checked"),
        "work investigate renders the history: {text}"
    );

    // ── the check on the conclusion ──────────────────────────────────
    let unlanded = event_trail(ctx, &fix).await?;
    ensure!(
        !unlanded.iter().any(|e| moved_to(e) == Some(Status::Done)),
        "a fix item that landed nothing was marked done"
    );
    ensure!(
        unlanded.iter().filter_map(moved_to).next_back()
            == Some(Status::Blocked(BlockedReason::NeedsDecision)),
        "it parks for its owner"
    );
    let question = wait_question(ctx, &fix).await?;
    ensure!(
        question["status"] == "open"
            && question["text"]
                .as_str()
                .is_some_and(|t| t.contains(INVESTIGATION_NOT_DONE) && t.contains(&fingerprint)),
        "its conclusion is the question the owner is asked: {question}"
    );
    ensure!(
        evidence(&parked)
            .iter()
            .any(|e| e["kind"] == "summary" && e["verified_by"].is_null()),
        "its findings are kept as evidence, unverified: {}",
        evidence_text(&parked)
    );
    // The item that waited on it goes on with its own ladder.
    let released = wait_for(ctx, &third, "the third item is released", |v| {
        events(v).iter().any(|e| {
            e["kind"] == "resume"
                && e["reason"]
                    .as_str()
                    .is_some_and(|r| r.contains(&fix) && r.contains("without landing a fix"))
        })
    })
    .await?;
    ensure!(
        !has_edge(&released, EdgeKind::Blocks, &fix),
        "the third item still waits on the parked fix item"
    );

    keep_evidence("work-investigate.json", &printed)?;
    keep_evidence("work-investigate.txt", &text)?;
    keep_evidence("daemon-log-fixture-line.unredacted.txt", &leaked)?;
    keep_evidence("daemon-log-slice.redacted.log", &slice)?;
    keep_evidence("fix-item-nothing-changed.events.txt", trail_text(&unlanded))?;
    keep_evidence(
        "fix-item-nothing-changed.events.json",
        serde_json::to_string_pretty(&unlanded)?,
    )?;
    keep_evidence(
        "fix-item-nothing-changed.question.json",
        serde_json::to_string_pretty(&question)?,
    )?;

    // ── answered, it lands a commit and is done ──────────────────────
    let question_id = question["id"].as_str().unwrap_or_default();
    submit_answer(ctx, question_id, INVESTIGATION_LAND_THE_FIX).await?;
    let landed = wait_status(ctx, &fix, Status::Done).await?;
    let commit = evidence(&landed)
        .into_iter()
        .find(|e| e["kind"] == "commit" && e["verified_by"] == "git")
        .ok_or_else(|| anyhow!("no commit git verified: {}", evidence_text(&landed)))?;
    let sha = commit["reference"].as_str().unwrap_or_default().to_string();
    ensure!(
        repo.git(&["cat-file", "-t", &sha]).ok().as_deref() == Some("commit"),
        "the landed commit exists in the repository: {sha}"
    );
    let done_trail = event_trail(ctx, &fix).await?;
    ensure!(
        done_trail.iter().filter_map(moved_to).next_back() == Some(Status::Done),
        "the fix item that landed a commit closes done"
    );
    keep_evidence("fix-item-landed-commit.events.txt", trail_text(&done_trail))?;
    keep_evidence(
        "fix-item-landed-commit.events.json",
        serde_json::to_string_pretty(&done_trail)?,
    )?;
    keep_evidence(
        "fix-item-landed-commit.evidence.json",
        serde_json::to_string_pretty(&evidence(&landed))?,
    )?;

    // ── the failure comes back ───────────────────────────────────────
    let fourth = file(ctx, failing("Add the fourth helper")).await?;
    let second_fix = wait_fix_item(ctx, &fourth, Some(&fix)).await?;
    let again = wait_status(
        ctx,
        &second_fix,
        Status::Blocked(BlockedReason::NeedsDecision),
    )
    .await?;
    ensure!(
        body(&again)["artifact_refs"]
            .as_array()
            .is_some_and(|refs| refs
                .iter()
                .any(|r| r["kind"] == "previous_fix" && r["value"] == fix.as_str())),
        "the second fix item names the first: {}",
        body(&again)["artifact_refs"]
    );
    // The first fix item's second run was capture 2; this is capture 3.
    let second_brief = std::fs::read_to_string(captures.join("run-3/brief.txt"))
        .context("the second fix item's brief was captured")?;
    for wanted in [
        format!("earlier fix: item #{fix} is done"),
        INVESTIGATION_LANDED.to_string(),
        format!("commit {sha}"),
        "The failure recurred after it: run ".to_string(),
        format!("of item #{fourth}"),
        "the fix was NOT in what ran".to_string(),
    ] {
        ensure!(
            second_brief.contains(&wanted),
            "the second fix item's brief lacks {wanted:?}: {second_brief}"
        );
    }
    let after: Value = get(ctx, &format!("{WORK}/investigate/{fingerprint}")).await?;
    let recurrences = after["fixes"]
        .as_array()
        .and_then(|fixes| fixes.iter().find(|f| f["item"]["id"] == fix.as_str()))
        .map(|f| f["recurrences"].clone())
        .unwrap_or_default();
    ensure!(
        recurrences.as_array().is_some_and(|all| all
            .iter()
            .any(|r| r["item"] == fourth.as_str() && r["includes_fix"] == "not_included")),
        "the history records the recurrence and that the fix was not in what ran: {recurrences}"
    );

    keep_evidence("second-fix-item.brief.txt", &second_brief)?;
    keep_evidence(
        "work-investigate.after-recurrence.json",
        serde_json::to_string_pretty(&after)?,
    )?;
    // What each investigator run recorded about what it was given: the
    // first fix item's two runs, then the second fix item's.
    for run in ["run-1", "run-2", "run-3"] {
        for file in [
            "brief.txt",
            "args.txt",
            "head.txt",
            "working-directory.txt",
            "read-index.txt",
            "read-session-log.txt",
        ] {
            keep_evidence_file(
                &format!("investigator/{run}/{file}"),
                &captures.join(run).join(file),
            )?;
        }
        keep_evidence_file(
            &format!("investigator/{run}/bundle/index.md"),
            &captures.join(run).join("bundle/index.md"),
        )?;
    }
    // The bundle as the controller wrote it for the first run.
    keep_evidence_file("bundle/bundle.json", &cap.join("bundle/bundle.json"))?;
    keep_evidence_file("bundle/index.md", &cap.join("bundle/index.md"))?;
    Ok(())
}

/// Run git in `dir`, with no user or system configuration, and return its
/// trimmed stdout.
fn git_in(dir: &std::path::Path, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()?;
    ensure!(
        out.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A fix item for a failure in the daemon's own code lands a commit in the
/// daemon's own source, on a daemon of its own whose only worker is the
/// investigation stand-in.
///
/// The daemon's source is modelled by a repository holding exactly the
/// commit the daemon under test was built from (`GET /api/version`), fetched
/// from this checkout by its full hash with no remote left behind, plus a
/// later commit on its `main`, so the checkout has moved on since the build.
/// The stand-in is registered for that repository, which is how the daemon
/// finds its own source (it holds the build's commit, with
/// `crates/rustykrab-control/Cargo.toml` in it). An item that names no
/// repository fails with an error no classifier recognises; its fix item is
/// scoped to the daemon's source, runs in a worktree of it at the build's
/// commit, and lands a commit there that git verifies. The scenario checks
/// the `workspace` and `commit` evidence rows a release slice will read, and
/// that nothing moved the source's `main`.
async fn own_source_fix(shared: &Ctx) -> Result<()> {
    let daemon = OwnDaemon::boot(shared).await?;
    let ctx = &daemon.ctx;
    let version = get(ctx, "/api/version").await?;
    let built = version["commit"]
        .as_str()
        .unwrap_or_default()
        .trim_end_matches("-dirty")
        .to_string();
    ensure!(
        !built.is_empty() && built != "unknown",
        "the daemon under test records no build commit ({}); this scenario needs a build \
         made from a git checkout",
        version["commit"]
    );
    let checkout = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let full = git_in(
        &checkout,
        &["rev-parse", "--verify", &format!("{built}^{{commit}}")],
    )
    .with_context(|| {
        format!(
            "the build's commit {built} is not in {}",
            checkout.display()
        )
    })?;
    let source = tempfile::Builder::new()
        .prefix("rustykrab-e2e-daemon-source-")
        .tempdir()?;
    git_in(source.path(), &["init", "-q", "--initial-branch=main", "."])?;
    git_in(
        source.path(),
        &[
            "fetch",
            "-q",
            "--depth",
            "1",
            &format!("file://{}", checkout.canonicalize()?.display()),
            &full,
        ],
    )?;
    git_in(source.path(), &["update-ref", "refs/heads/main", &full])?;
    let tree = git_in(source.path(), &["rev-parse", &format!("{full}^{{tree}}")])?;
    let later = git_in(
        source.path(),
        &[
            "-c",
            "user.name=e2e",
            "-c",
            "user.email=e2e@rustykrab.invalid",
            "commit-tree",
            &tree,
            "-p",
            &full,
            "-m",
            "later work on main, after the build",
        ],
    )?;
    git_in(source.path(), &["update-ref", "refs/heads/main", &later])?;
    ensure!(
        git_in(source.path(), &["remote"])?.is_empty(),
        "the stand-in source has a remote to push to"
    );

    let stand_in = ClaudeCodeStandIn::investigator()?;
    post(
        ctx,
        WORKERS,
        json!({
            "name": "pinch-own",
            "kind": "claude_code",
            "repos": [source.path()],
            "command": stand_in.path(),
        }),
    )
    .await?;
    let failing = file(
        ctx,
        draft(
            "task",
            "Reconcile the daemon's report",
            "[e2e-control own source]",
            INVESTIGATION_FAIL_IN_DAEMON,
        ),
    )
    .await?;
    let error = wait_error(ctx, &failing).await?;
    ensure!(
        error.class == ErrorClass::Unknown && error.detail.contains("E2E-OWN-SOURCE"),
        "the seeded failure is unclassified: {}/{} {}",
        error.class.as_str(),
        error.subclass.as_str(),
        error.detail
    );
    let fix = wait_fix_item(ctx, &failing, None).await?;
    let landed = wait_status(ctx, &fix, Status::Done).await?;

    // Scoped to the daemon's own source, and marked so.
    let resource = format!("repo:{}", source.path().display());
    ensure!(
        body(&landed)["writable_resources"] == json!([resource]),
        "the fix item is not scoped to the daemon's source: {}",
        body(&landed)["writable_resources"]
    );
    ensure!(
        body(&landed)["artifact_refs"]
            .as_array()
            .is_some_and(|refs| refs.iter().any(|r| r["kind"] == "daemon_source"
                && r["value"] == source.path().display().to_string().as_str())),
        "the fix item is not marked as a fix in the daemon's source: {}",
        body(&landed)["artifact_refs"]
    );

    // The two rows a release slice reads, exactly as a repository-scoped
    // item has them.
    let rows = evidence(&landed);
    let workspace = rows
        .iter()
        .rev()
        .find(|e| e["kind"] == "workspace")
        .cloned()
        .ok_or_else(|| anyhow!("no workspace row: {}", evidence_text(&landed)))?;
    let recorded: Value = serde_json::from_str(workspace["reference"].as_str().unwrap_or("{}"))?;
    ensure!(
        recorded["repo"] == source.path().display().to_string().as_str()
            && recorded["base"] == full.as_str()
            && workspace["hash"] == full.as_str()
            && workspace["verified_by"].is_null(),
        "the workspace row does not name the daemon's source at the build's commit {full}: \
         {workspace}"
    );
    let commit = rows
        .iter()
        .rev()
        .find(|e| e["kind"] == "commit" && e["verified_by"] == "git")
        .cloned()
        .ok_or_else(|| anyhow!("no commit row git verified: {}", evidence_text(&landed)))?;
    let sha = commit["reference"].as_str().unwrap_or_default().to_string();
    ensure!(
        sha.len() == 40 && commit["hash"].is_null(),
        "the commit row is not a full verified sha: {commit}"
    );
    ensure!(
        git_in(source.path(), &["rev-parse", &format!("{sha}^")])? == full,
        "the fix is not a commit on the build's commit"
    );
    let branch = recorded["branch"].as_str().unwrap_or_default();
    ensure!(
        git_in(
            source.path(),
            &["branch", "--contains", &sha, "--format=%(refname:short)"]
        )? == branch,
        "the fix commit is on more than its own branch {branch}"
    );
    ensure!(
        git_in(source.path(), &["rev-parse", "main"])? == later,
        "something moved the daemon source's main"
    );

    // What the investigator was given: a checkout of RustyKrab at the
    // build's commit, and a brief that says so.
    let cap = ctx.data_dir.join(INVESTIGATOR_CAPTURES).join("run-1");
    let head = std::fs::read_to_string(cap.join("head.txt"))?;
    ensure!(
        head.trim() == full,
        "the investigator ran at {}, not the build's commit {full}",
        head.trim()
    );
    let listing = std::fs::read_to_string(cap.join("working-directory.txt"))?;
    ensure!(
        listing.contains("crates") && listing.contains("Cargo.toml"),
        "the investigator's working directory is not RustyKrab's source: {listing}"
    );
    let brief = std::fs::read_to_string(cap.join("brief.txt"))?;
    for wanted in [
        "This failure is in RustyKrab's own code",
        "Nothing you commit is pushed, merged or deployed",
        INVESTIGATION_DAEMON_FAILURE,
    ] {
        ensure!(
            brief.contains(wanted),
            "the brief lacks {wanted:?}: {brief}"
        );
    }

    let trail = event_trail(ctx, &fix).await?;
    keep_evidence("own-source-fix.events.txt", trail_text(&trail))?;
    keep_evidence(
        "own-source-fix.evidence.json",
        serde_json::to_string_pretty(&rows)?,
    )?;
    keep_evidence(
        "own-source-fix.item.json",
        serde_json::to_string_pretty(body(&landed))?,
    )?;
    keep_evidence(
        "own-source-fix.git.txt",
        format!(
            "$ git rev-parse main\n{}\n$ git log --oneline -2 {branch}\n{}\n\
             $ git branch --contains {sha}\n{}\n$ git show --stat --format=%H%n%P%n%s {sha}\n{}\n",
            git_in(source.path(), &["rev-parse", "main"])?,
            git_in(source.path(), &["log", "--oneline", "-2", branch])?,
            git_in(
                source.path(),
                &["branch", "--contains", &sha, "--format=%(refname:short)"]
            )?,
            git_in(
                source.path(),
                &["show", "--stat", "--format=%H%n%P%n%s", &sha]
            )?,
        ),
    )?;
    keep_evidence_file(
        "own-source-fix.investigator/brief.txt",
        &cap.join("brief.txt"),
    )?;
    keep_evidence_file(
        "own-source-fix.investigator/working-directory.txt",
        &cap.join("working-directory.txt"),
    )?;
    Ok(())
}

// ── bounded recovery ─────────────────────────────────────────────────

/// The evidence rows of `kind` an item's detail carries, in order.
fn evidence_of(view: &Value, kind: &str) -> Vec<Value> {
    evidence(view)
        .into_iter()
        .filter(|e| e["kind"] == kind)
        .collect()
}

/// Observed run usage without a predicted allowance or parent reservation.
fn spend_report(ctx: &Ctx, id: &str) -> Result<(String, Value)> {
    let conn = read_only(ctx)?;
    let mut statement = conn.prepare(
        "SELECT COALESCE(run, ''), worker, tokens, wall_ms, iterations, at
           FROM work_spend WHERE item = ?1 ORDER BY at, rowid",
    )?;
    let rows: Vec<(String, String, i64, i64, i64, String)> = statement
        .query_map([id], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })?
        .collect::<Result<_, _>>()?;
    let mut text = format!("Observed work_spend for item {id} (no task allowance)\n");
    let (mut tokens, mut wall_ms, mut iterations) = (0i64, 0i64, 0i64);
    for (run, worker, t, w, i, at) in &rows {
        text.push_str(&format!(
            "{run}  {worker}  {t} tokens  {w} ms  {i} iterations  {at}\n"
        ));
        tokens += t;
        wall_ms += w;
        iterations += i;
    }
    text.push_str(&format!(
        "total: {} runs, {iterations} iterations, {tokens} tokens, {wall_ms} ms\n",
        rows.len()
    ));
    Ok((
        text,
        json!({"runs": rows.len(), "iterations": iterations, "tokens": tokens, "wall_ms": wall_ms}),
    ))
}

/// Every durable worker prompt explicitly records usage without task estimates.
fn usage_only_briefs(ctx: &Ctx, marker: &str) -> Result<usize> {
    let mut count = 0;
    for conversation in conversations_mentioning(ctx, marker)? {
        let text = strings(
            ctx,
            "SELECT data FROM messages WHERE conversation_id = ?1 ORDER BY idx",
            &conversation,
        )?
        .join("\n");
        ensure!(
            !text.contains("budget: "),
            "legacy step allowance in {conversation}"
        );
        if text.contains("Execution usage is recorded; there is no per-task token, iteration or runtime allowance.") {
            count += 1;
        }
    }
    Ok(count)
}

/// Bounded recovery, the repeated-outcome circuit breaker, on a daemon of
/// its own at the default `RUSTYKRAB_BREAKER_REPEATS` (3).
///
/// The incident's shape: on v6.5.6 one item ran at least 491 times and
/// 8.7 hours against a one-hour budget, revived again and again with
/// nothing changed and failing the same way each time. Here the reviver is
/// the item's own repair policy (ten repairs, and no improvement rung of
/// its own), and its worker fails every time with an error no classifier
/// recognises:
///
/// - the first attempt and three repairs fail; the last three ran in the
///   same attempt state with the same fingerprint, so the breaker trips:
///   no fifth identical attempt, a `circuit_open` row naming the three
///   runs, an `improve` rung that says so, and the item queued behind one
///   investigation filed through the ladder's own prerequisite path;
/// - the investigation lands a classifier rule that replays the failure,
///   which is a new repair generation: the item's next attempt state
///   differs, it resumes and reads the gauge;
/// - five observed runs, one investigation and a useful result.
async fn breaker_trips_once(shared: &Ctx) -> Result<()> {
    let daemon = OwnDaemon::boot_with(shared, &[]).await?;
    let ctx = &daemon.ctx;
    let tag = "[e2e-control bounded-recovery breaker]";
    let budget = json!({
        "iterations": 30, "tokens": 200_000, "wall_seconds": 600, "repairs": 10,
        "rungs": {
            "retries": 2, "repairs": 10, "worker_switches": 1, "acquisitions": 1,
            "builds": 1, "requests": 1, "improvements": 0, "replans": 1,
        },
    });
    let mut gauge = draft("personal", "Read the corrupted tide gauge", tag, W_GAUGE);
    gauge["budget"] = budget.clone();
    let x = file(ctx, gauge).await?;
    let done = wait_status(ctx, &x, Status::Done).await?;

    // ── the trip ──────────────────────────────────────────────────────
    // The item's ladder as the detail returns it (`rungs` would also read
    // each rung's event, so every rung twice).
    let climbed: Vec<String> = field(&done, "ladder")
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r["rung"].as_str().map(str::to_string))
        .collect();
    ensure!(
        climbed == ["repair", "repair", "repair", "improve"],
        "the ladder repaired three times, then the breaker filed the investigation: {climbed:?}"
    );
    let trail = event_trail(ctx, &x).await?;
    let tripped = trail
        .iter()
        .filter(|e| e["kind"] == "rung")
        .filter_map(|e| serde_json::from_str::<Value>(e["reason"].as_str()?).ok())
        .find(|r| r["rung"] == "improve")
        .ok_or_else(|| anyhow!("no improve rung"))?;
    let outcome = tripped["outcome"].as_str().unwrap_or_default().to_string();
    ensure!(
        outcome.contains("circuit breaker tripped: 3 failed attempts in a row"),
        "the improve rung says the breaker tripped: {outcome}"
    );
    let fingerprint = tripped["error"]["fingerprint"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    ensure!(
        tripped["error"]["class"] == "unknown",
        "the tripped failure is the worker's unrecognised one: {tripped}"
    );

    // ── what each attempt ran with ────────────────────────────────────
    let runs: Vec<String> = evidence_of(&done, "run")
        .iter()
        .filter_map(|e| e["reference"].as_str().map(str::to_string))
        .collect();
    let states = evidence_of(&done, "attempt_state");
    let hashes: Vec<&str> = states
        .iter()
        .map(|e| e["hash"].as_str().unwrap_or_default())
        .collect();
    ensure!(
        runs.len() == 5 && hashes.len() == 5,
        "five leases, each with its attempt state, well short of 491: {runs:?} {hashes:?}"
    );
    ensure!(
        hashes[1] == hashes[2] && hashes[2] == hashes[3],
        "the three repairs ran in one attempt state: {hashes:?}"
    );
    ensure!(
        hashes[0] != hashes[1] && hashes[4] != hashes[3],
        "the first attempt, and the one after the fix landed, are different states: {hashes:?}"
    );
    let open = evidence_of(&done, "circuit_open");
    ensure!(open.len() == 1, "one circuit_open row: {open:?}");
    let record: Value = serde_json::from_str(open[0]["reference"].as_str().unwrap_or_default())?;
    ensure!(
        record["runs"] == json!(runs[1..4]) && record["fingerprint"] == fingerprint.as_str(),
        "circuit_open names the three runs and the fingerprint: {record}"
    );
    for run in &runs[1..4] {
        ensure!(outcome.contains(run.as_str()), "the rung names run {run}");
    }
    ensure!(
        evidence_of(&done, "attempt_failure").len() == 4,
        "every judged failure is recorded with its state"
    );

    // ── one investigation, landed: a new repair generation ────────────
    let fixes: Vec<Value> = list(ctx, "kind=internal&include_closed=true")
        .await?
        .into_iter()
        .filter(|v| {
            body(v)["artifact_refs"].as_array().is_some_and(|refs| {
                refs.iter().any(|r| {
                    r["kind"] == "failure_fingerprint" && r["value"] == fingerprint.as_str()
                })
            })
        })
        .collect();
    ensure!(fixes.len() == 1, "exactly one investigation: {fixes:?}");
    let fix = id(&fixes[0]);
    ensure!(
        outcome.starts_with(&format!("waiting on resolution item {fix}")),
        "the item waited on that investigation: {outcome}"
    );
    let fixed = expect_status(ctx, &fix, Status::Done, "the investigation landed").await?;
    ensure!(
        evidence(&fixed)
            .iter()
            .any(|e| e["kind"] == "classifier_rule" && e["verified_by"] == "replay"),
        "its classifier rule replayed the failure: {}",
        evidence_text(&fixed)
    );
    let last_state: Value =
        serde_json::from_str(states[4]["reference"].as_str().unwrap_or_default())?;
    ensure!(
        last_state["generation"]
            .as_array()
            .is_some_and(|g| g.iter().any(|r| r["item"] == fix.as_str())),
        "the resumed attempt ran in the investigation's generation: {last_state}"
    );

    let (spend_text, totals) = spend_report(ctx, &x)?;
    ensure!(
        totals["runs"] == 5 && totals["iterations"].as_u64().unwrap_or(0) >= 5,
        "all five attempts retain observed usage: {totals}"
    );
    let given = usage_only_briefs(ctx, tag)?;
    ensure!(
        given == 5,
        "each of five attempts received usage-only instructions: {given}"
    );

    // ── the investigation record an operator reads ────────────────────
    let printed = cli_stdout(ctx, &["work", "investigate", &fingerprint])?;
    let as_json = cli_stdout(ctx, &["work", "investigate", &fingerprint, "--json"])?;
    let investigated: Value = serde_json::from_str(&as_json)?;
    ensure!(
        investigated["fixes"][0]["item"]["id"] == fix.as_str()
            && investigated["occurrences"][0]["item"]["id"] == x.as_str(),
        "work investigate shows the item and its one investigation: {}",
        excerpt(&as_json)
    );

    keep_evidence("breaker/item.events.txt", trail_text(&trail))?;
    keep_evidence(
        "breaker/item.events.json",
        serde_json::to_string_pretty(&trail)?,
    )?;
    keep_evidence(
        "breaker/item.attempt-state.json",
        serde_json::to_string_pretty(&states)?,
    )?;
    keep_evidence(
        "breaker/item.attempt-failure.json",
        serde_json::to_string_pretty(&evidence_of(&done, "attempt_failure"))?,
    )?;
    keep_evidence(
        "breaker/item.circuit-open.json",
        serde_json::to_string_pretty(&open)?,
    )?;
    keep_evidence("breaker/item.spend.txt", &spend_text)?;
    keep_evidence(
        "breaker/item.usage-only-briefs.txt",
        format!("{given} prompts without task execution allowances\n"),
    )?;
    keep_evidence(
        "breaker/investigation.events.txt",
        trail_text(&event_trail(ctx, &fix).await?),
    )?;
    keep_evidence("breaker/work-investigate.txt", &printed)?;
    keep_evidence("breaker/work-investigate.json", &as_json)?;
    Ok(())
}

/// Retired estimates cannot interrupt execution; an unchanged technical failure
/// still reaches a real decision, whose answer selects a working station.
async fn usage_only_decision_resumes(shared: &Ctx) -> Result<()> {
    let daemon = OwnDaemon::boot_with(shared, &[("RUSTYKRAB_BREAKER_REPEATS", "10")]).await?;
    let ctx = &daemon.ctx;
    let tag = "[e2e-control usage-only decision]";
    let mut station = draft("personal", "Query the gauge station", tag, W_STATION);
    station["budget"] = json!({"iterations": 2, "tokens": 1, "wall_seconds": 1, "repairs": 2});
    let x = file(ctx, station).await?;
    let held = wait_status(ctx, &x, Status::Blocked(BlockedReason::NeedsDecision)).await?;
    let error: rustykrab_core::work::WorkError =
        serde_json::from_value(field(&held, "last_error").clone())?;
    ensure!(
        error.class == ErrorClass::Tool && error.subclass.as_str() == "invalid_args",
        "the actual technical failure is preserved: {error:?}"
    );
    let leased = |view: &Value| events_of(view, EventKind::Lease).len();
    let runs_before = leased(&held);
    let (before_text, before) = spend_report(ctx, &x)?;
    ensure!(
        before["iterations"].as_u64().unwrap_or(0) > 2 && runs_before >= 3,
        "useful recovery passed the old two-iteration estimate: {before}"
    );
    tokio::time::sleep(QUIET).await;
    let still = expect_status(
        ctx,
        &x,
        Status::Blocked(BlockedReason::NeedsDecision),
        "technical decision remains held",
    )
    .await?;
    ensure!(
        leased(&still) == runs_before,
        "an unanswered technical question cannot requeue work"
    );
    let asked = questions_for(ctx, &x).await?;
    ensure!(asked.len() == 1, "one technical decision: {asked:?}");
    let question = asked[0].clone();
    ensure!(
        !question["text"]
            .as_str()
            .unwrap_or_default()
            .contains("budget is spent"),
        "no task allowance question: {question}"
    );
    let qid = question["id"].as_str().unwrap_or_default().to_string();
    let answer = submit_answer(ctx, &qid, ANSWER_STATION).await?;
    ensure!(
        answer["applied"]["answer_submission"]["status"] == "applied",
        "saved answer applied: {answer}"
    );
    let again = expect(
        ctx.post(
            &format!("{QUESTIONS}/{qid}/answer"),
            json!({"answer": ANSWER_STATION}),
        )
        .await?,
        &[409],
        "duplicate answer",
    )
    .await?;
    let done = wait_status(ctx, &x, Status::Done).await?;
    ensure!(
        evidence_of(&done, "budget_grant").is_empty(),
        "answer must not mint an execution grant"
    );
    ensure!(
        evidence_text(&done).contains("e2e-control-station-8"),
        "the corrected operation produced its result"
    );
    let (spend_text, totals) = spend_report(ctx, &x)?;
    ensure!(
        totals["runs"].as_u64() == Some(runs_before as u64 + 1)
            && totals["iterations"].as_u64() > before["iterations"].as_u64(),
        "one resumed run with cumulative observed usage: {totals}"
    );
    let given = usage_only_briefs(ctx, tag)?;
    ensure!(
        given == runs_before + 1,
        "every attempt received usage-only instructions: {given}"
    );
    let trail = event_trail(ctx, &x).await?;
    for (name, value) in [
        ("events", serde_json::to_value(&trail)?),
        ("question", question),
        ("answer", answer),
        ("second-answer", again),
        ("usage", totals),
    ] {
        keep_evidence(
            &format!("usage-only/item.{name}.json"),
            serde_json::to_string_pretty(&value)?,
        )?;
    }
    keep_evidence("usage-only/item.events.txt", trail_text(&trail))?;
    keep_evidence("usage-only/item.spend.before-answer.txt", before_text)?;
    keep_evidence("usage-only/item.spend.txt", spend_text)?;
    Ok(())
}

/// A scripted daemon sharing a data directory with another, killed when
/// the scenario ends however it ends.
struct CutoverDaemon {
    base: String,
    child: Option<std::process::Child>,
}

impl Drop for CutoverDaemon {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl CutoverDaemon {
    /// Boot a daemon on `dir`, keeping its log as `log` so a second boot
    /// on the same directory does not truncate the first one's.
    async fn boot(shared: &Ctx, dir: &std::path::Path, log: &str) -> Result<CutoverDaemon> {
        let port = crate::pick_free_port()?;
        let child = crate::spawn_daemon(&shared.bin, dir, port, None)?;
        // The daemon writes through its open handle, so the rename moves
        // the whole log, before and after.
        std::fs::rename(dir.join("daemon.log"), dir.join(log))?;
        let mut daemon = CutoverDaemon {
            base: format!("http://127.0.0.1:{port}"),
            child: Some(child),
        };
        let child = daemon.child.as_mut().expect("just spawned");
        crate::wait_for_health(&daemon.base, &shared.client, child)
            .await
            .with_context(|| {
                format!(
                    "{log}: {}",
                    std::fs::read_to_string(dir.join(log)).unwrap_or_default()
                )
            })?;
        Ok(daemon)
    }

    /// `GET <path>` on this daemon, as JSON.
    async fn get(&self, shared: &Ctx, path: &str) -> Result<Value> {
        Ok(shared
            .client
            .get(format!("{}{path}", self.base))
            .bearer_auth(crate::AUTH_TOKEN)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    /// The `controller` object of `GET /api/version`.
    async fn controller(&self, shared: &Ctx) -> Result<Value> {
        Ok(self.get(shared, "/api/version").await?["controller"].clone())
    }

    /// `controller.lock` as `GET /api/version` reports it: `"held"`,
    /// `"waiting"`, or `null` before the loop first tries the lock.
    async fn lock(&self, shared: &Ctx) -> Result<Value> {
        Ok(self.controller(shared).await?["lock"].clone())
    }

    /// `POST /api/work` on this daemon: file one item, returning its id.
    async fn file(&self, shared: &Ctx, draft: Value) -> Result<String> {
        let answer: Value = shared
            .client
            .post(format!("{}{WORK}", self.base))
            .bearer_auth(crate::AUTH_TOKEN)
            .json(&draft)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let outcome: PlanOutcome = serde_json::from_value(answer.clone()).with_context(|| {
            format!("POST {WORK} did not answer with a filing outcome: {answer}")
        })?;
        Ok(accepted(outcome)?.root)
    }

    /// Poll the item until it is done, for up to [`SETTLE`].
    async fn wait_done(&self, shared: &Ctx, id: &str) -> Result<Value> {
        let deadline = Instant::now() + SETTLE;
        loop {
            let view = self.get(shared, &format!("{WORK}/{id}")).await?;
            let now = status(&view)?;
            if now == Status::Done {
                return Ok(view);
            }
            ensure!(
                !now.is_closed() && Instant::now() < deadline,
                "{} never finished on {}; last {now}",
                title(&view),
                self.base
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// Poll until the lock reads `want`, for up to 30 seconds (the scripted
    /// daemon ticks every second).
    async fn wait_lock(&self, shared: &Ctx, want: &str) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let lock = self.lock(shared).await?;
            if lock == want {
                return Ok(());
            }
            ensure!(
                Instant::now() < deadline,
                "{} never reported controller.lock {want:?}; last {lock}",
                self.base
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn stop(mut self) {
        if let Some(child) = self.child.take() {
            crate::shutdown_daemon(child).await;
        }
    }
}

/// An update's cutover: two daemons on one data directory. Exactly one
/// holds `controller.lock` and ticks, the other reports `waiting` on
/// `GET /api/version`, and once the holder stops the waiting one takes
/// the lock and reports `held`. The waiting one never ticks: an item filed
/// through it is run to done by the holder while its `last_tick` stays
/// null. Boots its own pair, since stopping the shared daemon would take
/// it from every scenario after this one.
async fn two_daemons_one_data_dir(shared: &Ctx) -> Result<()> {
    let dir = tempfile::Builder::new()
        .prefix("rustykrab-e2e-cutover-")
        .tempdir()?;
    let first = CutoverDaemon::boot(shared, dir.path(), "daemon-first.log").await?;
    first.wait_lock(shared, "held").await?;

    let second = CutoverDaemon::boot(shared, dir.path(), "daemon-second.log").await?;
    second.wait_lock(shared, "waiting").await?;
    // Give the waiting loop several more ticks: it must keep waiting, and
    // the holder must keep holding, while both run.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let (a, b) = (first.lock(shared).await?, second.lock(shared).await?);
    ensure!(
        a == "held" && b == "waiting",
        "with both running, expected held/waiting, got {a}/{b}"
    );

    // File through the waiting daemon: only the holder's loop may lease
    // and run it, and the waiting loop must not complete a single tick.
    let errand = second
        .file(
            shared,
            draft("personal", "File during the cutover", &tag(0), W_SUCCEED),
        )
        .await?;
    let finished = first.wait_done(shared, &errand).await?;
    ensure!(
        worker_of(&finished).is_some(),
        "the item filed through the waiting daemon finished without a lease: {finished}"
    );
    let (held, waited) = (
        first.controller(shared).await?,
        second.controller(shared).await?,
    );
    ensure!(
        !held["last_tick"].is_null(),
        "the holder reports no tick: {held}"
    );
    ensure!(
        waited["lock"] == "waiting" && waited["last_tick"].is_null(),
        "the waiting daemon ticked while another held controller.lock: {waited}"
    );

    first.stop().await;
    second.wait_lock(shared, "held").await?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let took_over = second.controller(shared).await?;
        if !took_over["last_tick"].is_null() {
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "the second daemon holds the lock but never ticked: {took_over}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    second.stop().await;
    Ok(())
}

/// Scenario 13: A worker fails on a tool that does not exist; the ladder files a
/// `capability` build item, the tool is built and verified, the original
/// item resumes with it activated, and the user is never asked.
async fn s13(ctx: &Ctx) -> Result<()> {
    let tag = tag(13);
    let repo = FixtureRepo::create()?;
    let stand_in = ClaudeCodeStandIn::create()?;
    register_claude_code(ctx, "pinch-s13", &repo, &stand_in).await?;
    let task = file(
        ctx,
        draft(
            "personal",
            "Check the tides for Saturday's sail",
            &tag,
            W_TIDE,
        ),
    )
    .await?;
    let waiting = wait_for(ctx, &task, "the task waits on a capability item", |v| {
        edges(v).iter().any(|e| e["kind"] == "blocks")
    })
    .await?;
    let capability = edges(&waiting)
        .iter()
        .find(|e| e["kind"] == "blocks")
        .and_then(|e| e["depends_on"].as_str())
        .map(str::to_string)
        .unwrap_or_default();
    let build = item(ctx, &capability).await?;
    ensure!(
        body(&build)["kind"] == "capability",
        "the ladder filed a {} item, want a capability build",
        body(&build)["kind"]
    );
    wait_status(ctx, &capability, Status::Done).await?;
    wait_status(ctx, &task, Status::Done).await?;
    let runs = worker_runs(ctx, &tag)?;
    let resumed = runs.last().map(active_at_first_step).unwrap_or_default();
    ensure!(
        resumed.iter().any(|tool| tool == "tide_table"),
        "the task resumed without the built tool active: {resumed:?}"
    );
    ensure!(
        questions_for(ctx, &task).await?.is_empty()
            && questions_for(ctx, &capability).await?.is_empty(),
        "the user was asked"
    );
    Ok(())
}

// ── Phase 4: questions, blocked states and standing judgment ─────────

/// Scenario 3: A worker reports `needs_decision`; the question reaches the user's
/// channel; the answer resumes the item, not the conversation; a
/// defaultable question never reaches the user.
async fn s03(ctx: &Ctx) -> Result<()> {
    let tag = tag(3);
    let conversation = telegram_thread(ctx, 103).await?;
    let mut flowers = draft("personal", "Order flowers for Mum", &tag, W_ASK);
    flowers["origin_conversation_id"] = json!(conversation);
    let task = file(ctx, flowers).await?;
    wait_status(ctx, &task, Status::Blocked(BlockedReason::NeedsDecision)).await?;
    let question = wait_question(ctx, &task).await?;
    ensure!(
        question["class"] == "blocking_now",
        "the router did not class the question blocking now: {question}"
    );
    wait_told(ctx, FLORIST_QUESTION).await?;
    let question_id = question["id"].as_str().unwrap_or_default();
    submit_answer(ctx, question_id, ANSWER_FLORIST).await?;
    wait_status(ctx, &task, Status::Done).await?;
    let carried = conversations_mentioning(ctx, ANSWER_FLORIST)?;
    ensure!(
        carried.iter().any(|c| c != &conversation) && !carried.contains(&conversation),
        "the answer resumed the conversation instead of the item: {carried:?}"
    );

    let mut reminder = draft("personal", "Set the dentist reminder", &tag, W_ASK_DEFAULT);
    reminder["origin_conversation_id"] = json!(conversation);
    let reminder = file(ctx, reminder).await?;
    let recorded = wait_question(ctx, &reminder).await?;
    ensure!(
        recorded["class"] == "defaultable",
        "the router did not class the question defaultable: {recorded}"
    );
    tokio::time::sleep(QUIET).await;
    ensure!(
        told(ctx, DEFAULTABLE_QUESTION)?.is_empty(),
        "the defaultable question reached the user"
    );
    Ok(())
}

/// Scenario 5: A worker stalls; bounded recovery starts another attempt while
/// carrying the first attempt's evidence without an owner grant.
/// An executor scripted to keep looping ends at a finite technical hold.
async fn s05(ctx: &Ctx) -> Result<()> {
    let tag = tag(5);
    let task = file(
        ctx,
        draft("personal", "Tidy the downloads folder", &tag, W_STALL),
    )
    .await?;
    let repaired = wait_for(ctx, &task, "the stall triggers repair", |v| {
        climbed(v, Rung::Repair)
    })
    .await?;
    let trigger = events_of(&repaired, EventKind::Rung)
        .into_iter()
        .map(|e| e.to_string().to_lowercase())
        .find(|e| e.contains("repair"))
        .unwrap_or_default();
    ensure!(
        trigger.contains("stall") || trigger.contains("progress"),
        "the repair was not triggered by the progress ledger: {trigger}"
    );
    // The fixture never changes approach. Its progress ledger and finite
    // recovery rungs stop that loop independently of estimated task effort.
    let held = wait_status(ctx, &task, Status::Blocked(BlockedReason::NeedsDecision)).await?;
    let (spend_text, spend) = spend_report(ctx, &task)?;
    let runs = spend["runs"].as_u64().unwrap_or(0);
    ensure!(
        (2..=6).contains(&runs) && spend["iterations"].as_u64().unwrap_or(u64::MAX) <= runs * 26,
        "persistent loops hand off within the fingerprint-ledger protocol: {spend_text}"
    );
    ensure!(
        evidence_of(&held, "budget_grant").is_empty(),
        "recovery must not invent an owner grant"
    );
    let deadline = Instant::now() + SETTLE;
    let attempts = loop {
        let attempts = conversations_mentioning(ctx, &tag)?;
        if attempts.len() >= 2 {
            break attempts;
        }
        ensure!(
            Instant::now() < deadline,
            "no second attempt without a budget grant after {SETTLE:?}"
        );
        tokio::time::sleep(POLL).await;
    };
    let mut first_evidence: Vec<String> = evidence(&repaired)
        .iter()
        .filter_map(|e| e["reference"].as_str().map(str::to_string))
        .collect();
    first_evidence.push(attempts[0].clone());
    let second_brief = strings(
        ctx,
        "SELECT data FROM messages WHERE conversation_id = ?1 ORDER BY idx",
        &attempts[1],
    )?
    .join("\n");
    ensure!(
        first_evidence.iter().any(|r| second_brief.contains(r.as_str())),
        "the second attempt's brief carries none of the first attempt's evidence: {first_evidence:?}"
    );
    Ok(())
}

/// Scenario 18: A multi-step personal request is routed to the `planner`, which
/// starts with `work_plan`, `work_status`, `memory_search` and
/// `recall_search`. Its first `work_plan` has a cycle through a parent link
/// and is rejected whole with `cycle` and the offending temp ids, leaving
/// nothing stored; the corrected call is accepted with real ids, and only
/// then is anything leased.
async fn s18(ctx: &Ctx) -> Result<()> {
    let planner = workers(ctx)
        .await?
        .into_iter()
        .find(|w| w["name"] == "planner")
        .ok_or_else(|| anyhow!("no planner worker is registered"))?;
    let tools = if planner["capabilities"]["tools"].is_array() {
        planner["capabilities"]["tools"].clone()
    } else {
        planner["tools"].clone()
    };
    for tool in ["work_plan", "work_status", "memory_search", "recall_search"] {
        ensure!(
            tools
                .as_array()
                .is_some_and(|t| t.iter().any(|n| n == tool)),
            "the planner does not start with {tool} visible: {tools}"
        );
    }
    telegram_thread(ctx, 118).await?;
    telegram_say(ctx, 118, S18_REQUEST).await?;
    wait_titled(ctx, S18_TITLE).await?;
    let deadline = Instant::now() + SETTLE;
    let run = 'found: loop {
        for conversation in conversations_mentioning(ctx, S18_PLANNER)? {
            let run = Transcript::from_store(&ctx.db_path, &conversation)?;
            let answered = run
                .executed("work_plan")
                .iter()
                .filter(|c| c.output.is_some())
                .count();
            if answered >= 2 {
                break 'found run;
            }
        }
        ensure!(
            Instant::now() < deadline,
            "the planner made no two work_plan calls"
        );
        tokio::time::sleep(POLL).await;
    };
    let calls = run.executed("work_plan");
    let outcome = |n: usize| -> Result<PlanOutcome> {
        let output = calls[n].output.clone().unwrap_or_default();
        serde_json::from_value(output.clone())
            .with_context(|| format!("work_plan answered with no filing outcome: {output}"))
    };
    let first = rejected(outcome(0)?)?;
    let cycle = first
        .failed
        .iter()
        .find(|check| check.reason == RejectionReason::Cycle)
        .ok_or_else(|| {
            anyhow!(
                "the first graph was not rejected with cycle: {:?}",
                reasons(&first)
            )
        })?;
    for tmp in ["flights", "hotel"] {
        ensure!(
            cycle
                .offending
                .contains(&rustykrab_core::work::ItemRef::Tmp {
                    tmp: tmp.to_string()
                }),
            "the cycle does not name {tmp}: {:?}",
            cycle.offending
        );
    }
    ensure!(
        find_titled(ctx, S18_DRAFT_ONLY).await?.is_none(),
        "an item from the rejected graph exists in the store"
    );
    let second = accepted(outcome(1)?)?;
    ensure!(
        second.ids.len() == 4,
        "the corrected graph was not accepted with real ids: {:?}",
        second.ids
    );
    let mut graph_items = Vec::new();
    for real in second.ids.values() {
        graph_items.push(item(ctx, real).await?);
    }
    let accepted_at = graph_items
        .iter()
        .filter_map(|v| time(&body(v)["created_at"]))
        .min()
        .ok_or_else(|| anyhow!("the accepted items carry no creation time"))?;
    for view in graph_items.iter().filter(|v| was_leased(v)) {
        ensure!(
            leased_at(view)? >= accepted_at,
            "{} was leased before the graph was accepted",
            title(view)
        );
    }
    Ok(())
}

/// Scenario 29: A plan over the approval threshold is accepted with the triggering
/// items held; the preview reaches the phone; held items wait in
/// `blocked(needs_consent)` while unheld siblings run; `/approve` releases
/// them and `/reject` cancels them with cascade. A plan under the threshold
/// is leased at once, the decision recorded, and the user is not asked.
async fn s29(ctx: &Ctx) -> Result<()> {
    let conversation = telegram_thread(ctx, 129).await?;
    let birthday = |tag: &str| {
        let mut invite = node("m", "P", "personal", "Invite Ana's friends", tag, W_SUCCEED);
        invite["writable_resources"] = json!(["message:third_party"]);
        graph(
            "P",
            vec![
                root_node("P", "Plan Ana's birthday", tag),
                node("a", "P", "personal", "Pick the restaurant", tag, W_SUCCEED),
                invite,
                node(
                    "n",
                    "P",
                    "personal",
                    "Confirm the headcount",
                    tag,
                    W_SUCCEED,
                ),
            ],
            vec![edge("n", EdgeKind::Blocks, "m")],
            Some(&conversation),
        )
    };
    let first = tag(29);
    let filed = accepted(plan(ctx, birthday(&first)).await?)?;
    let [p, a, m, n] = ids(&filed, ["P", "a", "m", "n"])?;
    ensure!(
        filed.held.contains(&m) && filed.held.contains(&n) && !filed.held.contains(&a),
        "held is not the third-party message and what depends on it: {:?}",
        filed.held
    );
    wait_told(ctx, &format!("Invite Ana's friends {first}")).await?;
    wait_status(ctx, &m, Status::Blocked(BlockedReason::NeedsConsent)).await?;
    wait_status(ctx, &a, Status::Done).await?;
    expect_status(
        ctx,
        &m,
        Status::Blocked(BlockedReason::NeedsConsent),
        "the held item waits while its unheld sibling runs",
    )
    .await?;
    telegram_say(ctx, 129, &format!("/approve {p}")).await?;
    wait_status(ctx, &m, Status::Done).await?;
    wait_status(ctx, &n, Status::Done).await?;

    let second = tag(29);
    let filed = accepted(plan(ctx, birthday(&second)).await?)?;
    let [p, _, m, n] = ids(&filed, ["P", "a", "m", "n"])?;
    wait_status(ctx, &m, Status::Blocked(BlockedReason::NeedsConsent)).await?;
    telegram_say(ctx, 129, &format!("/reject {p} not this year")).await?;
    wait_status(ctx, &m, Status::Cancelled(CancelReason::Requested)).await?;
    wait_for(
        ctx,
        &n,
        "what depends on a rejected item is cancelled",
        |v| status(v).is_ok_and(|s| s.name() == "cancelled"),
    )
    .await?;

    let third = tag(29);
    let small = file_graph(
        ctx,
        vec![
            root_node("P", "Plan a quiet dinner", &third),
            node(
                "a",
                "P",
                "personal",
                "Pick the restaurant",
                &third,
                W_SUCCEED,
            ),
        ],
        vec![],
        Some(&conversation),
    )
    .await?;
    ensure!(
        small.held.is_empty(),
        "a plan under the threshold held {:?}",
        small.held
    );
    let [p, a] = ids(&small, ["P", "a"])?;
    wait_status(ctx, &a, Status::Done).await?;
    ensure!(
        events(&item(ctx, &p).await?)
            .iter()
            .any(|e| e["actor"] == "policy"),
        "the delegated decision was not recorded"
    );
    ensure!(
        questions_for(ctx, &p).await?.is_empty(),
        "the user was asked"
    );
    Ok(())
}

// ── Phase 5: peer workers over the tailnet ───────────────────────────

/// Scenario 4: The daemon restarts mid-lease; the item resumes from its checkpoint
/// or returns to `ready`; nothing is marked failed by the restart alone.
async fn s04(ctx: &Ctx) -> Result<()> {
    let tag = tag(4);
    let task = file(
        ctx,
        draft("personal", "Sort the photo library", &tag, W_SLOW),
    )
    .await?;
    wait_for(ctx, &task, "the item is leased", is_active).await?;
    ctx.restart_daemon().await?;
    let now = status(&item(ctx, &task).await?)?;
    ensure!(
        now == Status::Ready || now.is_active() || now == Status::Done,
        "the item neither resumed nor returned to ready: {now}"
    );
    let finished = wait_status(ctx, &task, Status::Done).await?;
    ensure!(
        !events(&finished)
            .iter()
            .any(|e| moved_to(e) == Some(Status::Failed)),
        "the restart alone marked the item failed"
    );
    Ok(())
}

/// A second daemon as the peer: its own data dir and port, killed when the
/// scenario ends however it ends.
struct PeerDaemon {
    child: Option<std::process::Child>,
    dir: tempfile::TempDir,
    port: u16,
    base: String,
}

impl Drop for PeerDaemon {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl PeerDaemon {
    async fn boot(ctx: &Ctx, port: u16) -> Result<PeerDaemon> {
        let dir = tempfile::Builder::new()
            .prefix("rustykrab-e2e-peer-")
            .tempdir()?;
        let mut peer = PeerDaemon {
            child: None,
            dir,
            port,
            base: format!("http://127.0.0.1:{port}"),
        };
        peer.start(ctx).await?;
        Ok(peer)
    }

    async fn start(&mut self, ctx: &Ctx) -> Result<()> {
        let mut child = crate::spawn_daemon(&ctx.bin, self.dir.path(), self.port, None)?;
        crate::wait_for_health(&self.base, &ctx.client, &mut child).await?;
        self.child = Some(child);
        Ok(())
    }

    /// Kill the peer outright, mid-task, as a crash would, and boot it again
    /// on the same store and port.
    async fn crash_and_restart(&mut self, ctx: &Ctx) -> Result<()> {
        if let Some(mut child) = self.child.take() {
            child.kill()?;
            child.wait()?;
        }
        self.start(ctx).await
    }

    fn db(&self) -> std::path::PathBuf {
        self.dir.path().join("db").join("store.db")
    }

    /// The peer's delegated tasks for a controller's work item: status and
    /// how often the peer claimed each.
    fn tasks_for(&self, item: &str) -> Result<Vec<(String, i64)>> {
        let conn = rusqlite::Connection::open_with_flags(
            self.db(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let mut statement = conn.prepare(
            "SELECT status, attempts FROM delegated_tasks WHERE work_item_id = ?1 \
             ORDER BY created_at",
        )?;
        let rows = statement
            .query_map([item], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    /// Wait until the peer is running its task for `item`.
    async fn wait_running(&self, item: &str) -> Result<()> {
        let deadline = Instant::now() + SETTLE;
        loop {
            if self
                .tasks_for(item)
                .unwrap_or_default()
                .iter()
                .any(|(status, _)| status == "running")
            {
                return Ok(());
            }
            ensure!(
                Instant::now() < deadline,
                "the peer never ran a task for #{item}: {:?}",
                self.tasks_for(item)
            );
            tokio::time::sleep(POLL).await;
        }
    }
}

/// Whether the item's history moved it to `failed`.
fn ever_failed(view: &Value) -> bool {
    events(view)
        .iter()
        .any(|e| moved_to(e) == Some(Status::Failed))
}

/// Scenario 8: A peer node receives `required_tools`, activates them before its
/// first prefill, and returns a structured result.
///
/// The Phase 5 exit goes further, and so does this driver: the item survives
/// a restart on either side. The peer is registered before it boots (it is
/// leasable once the registry reads its advertisement), runs a probe item
/// with `caldav` required, then is killed in the middle of a second item's
/// run (its task goes back to its queue and the controller sees it through),
/// and the controller restarts in the middle of a third (the lease is
/// re-checked against the peer's task and re-attached, not resubmitted).
/// Scenario 4 is the local half of the same rule.
async fn s08(ctx: &Ctx) -> Result<()> {
    // The peer is the available executor in this fixture. Backend selectors
    // are retired and cannot force work away from another compatible resource.
    let daemon = OwnDaemon::boot(ctx).await?;
    let ctx = &daemon.ctx;
    let tag = tag(8);
    let port = crate::pick_free_port()?;
    let base = format!("http://127.0.0.1:{port}");
    post(
        ctx,
        WORKERS,
        json!({ "name": "krabby", "kind": "peer", "base_url": base, "token": crate::AUTH_TOKEN }),
    )
    .await?;
    let mut peer = PeerDaemon::boot(ctx, port).await?;

    // Required tools active before the first prefill, a structured result.
    let mut task = draft("personal", "Check the harbour calendar", &tag, W_PROBE);
    task["required_tools"] = json!(["caldav"]);
    let task = file(ctx, task).await?;
    let finished = wait_status(ctx, &task, Status::Done).await?;
    ensure!(
        worker_of(&finished).as_deref() == Some("krabby"),
        "the item ran on {:?}, not the peer",
        worker_of(&finished)
    );
    ensure!(
        evidence_text(&finished).contains("e2e-control-evidence"),
        "the peer's structured result did not become evidence: {}",
        evidence_text(&finished)
    );
    let peer_db = peer.db();
    let conn = rusqlite::Connection::open_with_flags(
        &peer_db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let mut statement = conn.prepare(
        "SELECT conversation_id FROM messages WHERE instr(data, ?1) > 0 GROUP BY conversation_id",
    )?;
    let conversations: Vec<String> = statement
        .query_map([&tag], |row| row.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    let active: Vec<String> = conversations
        .iter()
        .filter_map(|c| Transcript::from_store(&peer_db, c).ok())
        .flat_map(|run| active_at_first_step(&run))
        .collect();
    ensure!(
        active.iter().any(|tool| tool == "caldav"),
        "the peer did not have caldav active at its first step: {active:?}"
    );
    let workers = get(ctx, WORKERS).await?;
    let krabby = workers["workers"]
        .as_array()
        .and_then(|all| all.iter().find(|w| w["name"] == "krabby"))
        .cloned()
        .unwrap_or_default();
    ensure!(
        krabby["capabilities"]["tools"]
            .as_array()
            .is_some_and(|t| t.iter().any(|n| n == "caldav")),
        "the registry did not record the peer's advertisement: {krabby}"
    );
    ensure!(
        !krabby.to_string().contains(crate::AUTH_TOKEN),
        "the registry shows the peer's token"
    );

    // The peer restarts mid-run: its task returns to its queue and runs
    // again, and nothing is failed by the restart.
    let mut inbox = draft("personal", "Tidy the peer's inbox", &tag, W_PEER_PAUSE);
    inbox["required_tools"] = json!(["caldav"]);
    let inbox = file(ctx, inbox).await?;
    peer.wait_running(&inbox).await?;
    peer.crash_and_restart(ctx).await?;
    let finished = wait_status(ctx, &inbox, Status::Done).await?;
    ensure!(
        !ever_failed(&finished),
        "the peer's restart failed the item"
    );
    let tasks = peer.tasks_for(&inbox)?;
    ensure!(
        tasks.len() == 1 && tasks[0].1 >= 2 && tasks[0].0 == "done",
        "the peer did not run its one task again after restarting: {tasks:?}"
    );

    // The controller restarts mid-run: the lease is re-checked against the
    // peer's task, which still runs, and the controller re-attaches to it.
    let mut photos = draft("personal", "Sort the peer's photos", &tag, W_PEER_PAUSE);
    photos["required_tools"] = json!(["caldav"]);
    let photos = file(ctx, photos).await?;
    peer.wait_running(&photos).await?;
    ctx.restart_daemon().await?;
    let finished = wait_status(ctx, &photos, Status::Done).await?;
    ensure!(
        !ever_failed(&finished),
        "the controller's restart failed the item"
    );
    let history = events(&finished);
    let reattached = history.iter().any(|e| {
        e["kind"] == EventKind::Resume.as_str()
            && e["reason"]
                .as_str()
                .is_some_and(|r| r.contains("re-attached"))
    });
    ensure!(
        reattached,
        "the controller did not re-attach to the peer's run: {}",
        excerpt(&Value::Array(history.clone()).to_string())
    );
    ensure!(
        !history
            .iter()
            .any(|e| moved_to(e) == Some(Status::Ready) && e["kind"] == EventKind::Resume.as_str()),
        "the restart returned the peer's item to ready"
    );
    let tasks = peer.tasks_for(&photos)?;
    ensure!(
        tasks.len() == 1,
        "the controller submitted the item again instead of re-attaching: {tasks:?}"
    );
    drop(peer);
    Ok(())
}

// ── Phase 6: proposals from dreaming ─────────────────────────────────

fn labels(issue: &Value) -> Vec<String> {
    issue["labels"]
        .as_array()
        .map(|all| {
            all.iter()
                .filter_map(|l| {
                    l.as_str()
                        .or_else(|| l["name"].as_str())
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn seed_outcomes(
    ctx: &Ctx,
    skill: &str,
    signal: rustykrab_core::outcome::SignalClass,
) -> Result<()> {
    use rustykrab_core::outcome::{Attribution, OutcomeRecord, OutcomeVerdict};
    let store = open_store(ctx)?;
    for _ in 0..6 {
        let mut record = OutcomeRecord::new(
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            OutcomeVerdict::Failure,
            signal,
        );
        record.attributions.push(Attribution::skill(skill));
        record.detail = Some(format!("e2e-control: {skill} failed"));
        store.outcomes().record(&record).await?;
    }
    Ok(())
}

/// Scenario 7: Dreaming emits a proposal from a verifiable signal only; it appears as
/// a labelled issue; acceptance on the issue creates a `code` item; an
/// implicit signal produces no proposal.
async fn s07(ctx: &Ctx) -> Result<()> {
    use rustykrab_core::outcome::SignalClass;
    let tag = tag(7);
    let nonce = tag
        .trim_end_matches(']')
        .rsplit(' ')
        .next()
        .unwrap_or_default();
    let verifiable = format!("e2e-control-verifiable-{nonce}");
    let implicit = format!("e2e-control-implicit-{nonce}");
    seed_outcomes(ctx, &verifiable, SignalClass::Verifiable).await?;
    seed_outcomes(ctx, &implicit, SignalClass::Implicit).await?;
    evaluate(ctx).await?;
    let proposals = proposals_mentioning(ctx, &verifiable).await?;
    ensure!(
        proposals.len() == 1,
        "want one proposal from the verifiable signal, got {}",
        proposals.len()
    );
    ensure!(
        proposals_mentioning(ctx, &implicit).await?.is_empty(),
        "an implicit signal produced a proposal"
    );
    let github = &stand_ins(ctx)?.github;
    let issue = github
        .issues()
        .into_iter()
        .find(|i| i.to_string().contains(&verifiable))
        .ok_or_else(|| anyhow!("the proposal was not projected to an issue"))?;
    ensure!(
        labels(&issue).iter().any(|l| l == "rustykrab-proposal"),
        "the proposal's issue is not labelled rustykrab-proposal: {:?}",
        labels(&issue)
    );
    let mut accepted_labels = issue["labels"].as_array().cloned().unwrap_or_default();
    accepted_labels.push(json!("rustykrab-accepted"));
    github.hand_edit(
        issue["number"].as_u64().unwrap_or_default(),
        "labels",
        Value::Array(accepted_labels),
    )?;
    evaluate(ctx).await?;
    let proposal = id(&proposals[0]);
    let code = wait_listed(
        ctx,
        "kind=code",
        "a code item from the accepted proposal",
        |v| v.to_string().contains(&proposal),
    )
    .await?;
    ensure!(
        has_edge(&code, EdgeKind::DiscoveredFrom, &proposal),
        "the code item does not name the proposal it came from: {:?}",
        edges(&code)
    );
    Ok(())
}

/// Scenario 12: A proposal that would modify the controller, a policy or its own
/// measurement is refused below the highest review tier.
async fn s12(ctx: &Ctx) -> Result<()> {
    let tag = tag(12);
    for subject in ["controller", "policy", "measurement"] {
        let mut proposal = draft(
            "proposal",
            &format!("Change the {subject}"),
            &tag,
            "Proposal",
        );
        proposal["subject"] = json!(subject);
        proposal["review_tier"] = json!("standard");
        let refused = rejected(filing(ctx, WORK, proposal.clone()).await?)?;
        ensure!(
            reasons(&refused).contains(&RejectionReason::OutOfScope),
            "a proposal touching the {subject} was not refused below the highest tier: {:?}",
            reasons(&refused)
        );
        proposal["review_tier"] = json!("highest");
        accepted(filing(ctx, WORK, proposal).await?)
            .with_context(|| format!("the {subject} proposal at the highest tier"))?;
    }
    Ok(())
}

/// Scenario 15: A surfaced question whose answer was a recorded default is flagged
/// by dreaming as an avoidable escalation and produces a proposal.
async fn s15(ctx: &Ctx) -> Result<()> {
    let tag = tag(15);
    let conversation = telegram_thread(ctx, 115).await?;
    let mut check_in = draft(
        "personal",
        "Schedule the weekly check-in",
        &tag,
        W_ASK_SURFACED,
    );
    check_in["origin_conversation_id"] = json!(conversation);
    let task = file(ctx, check_in).await?;
    let question = wait_question(ctx, &task).await?;
    wait_told(ctx, "e2e-control check-in").await?;
    let question_id = question["id"].as_str().unwrap_or_default();
    submit_answer(ctx, question_id, "9am").await?;
    evaluate(ctx).await?;
    let flagged = proposals_mentioning(ctx, &task).await?;
    ensure!(
        flagged
            .iter()
            .any(|p| p.to_string().to_lowercase().contains("avoidable")),
        "no proposal flags the escalation as avoidable"
    );
    Ok(())
}

async fn metric_rows(ctx: &Ctx) -> Result<Vec<Value>> {
    let answer = get(ctx, WORK_METRICS).await?;
    let rows = if answer.is_array() {
        &answer
    } else {
        &answer["metrics"]
    };
    rows.as_array()
        .cloned()
        .ok_or_else(|| anyhow!("GET {WORK_METRICS} is not a list: {answer}"))
}

/// Scenario 16: Each expectation metric in section 1.1 is computed nightly from
/// stored events, and a regression produces a proposal naming the metric.
async fn s16(ctx: &Ctx) -> Result<()> {
    let tag = tag(16);
    evaluate(ctx).await?;
    let metrics = metric_rows(ctx).await?;
    ensure!(
        metrics.len() >= 8,
        "{} metrics computed, want one per expectation of section 1.1",
        metrics.len()
    );
    ensure!(
        metrics
            .iter()
            .all(|m| m["name"].is_string() && !m["value"].is_null()),
        "a metric has no name or no computed value: {metrics:?}"
    );
    for n in 1..=2 {
        let ledger = file(
            ctx,
            draft(
                "personal",
                &format!("Reconcile ledger {n}"),
                &tag,
                W_UNRECOGNISED,
            ),
        )
        .await?;
        wait_error(ctx, &ledger).await?;
    }
    evaluate(ctx).await?;
    let unknown = metric_rows(ctx)
        .await?
        .into_iter()
        .find_map(|m| {
            m["name"]
                .as_str()
                .filter(|n| n.contains("unknown"))
                .map(str::to_string)
        })
        .ok_or_else(|| anyhow!("no unknown-error metric"))?;
    ensure!(
        !proposals_mentioning(ctx, &unknown).await?.is_empty(),
        "the regression produced no proposal naming {unknown}"
    );
    Ok(())
}

/// Scenario 27: `personal` and `research` items run to completion with no issue; a
/// proposal is a `rustykrab-proposal` issue, a capability build is an issue,
/// a credential acquisition is not; a proposal's `personal` input shows only
/// as `local:#N`, and because the proposal names a personal item its own
/// title and text are withheld too (plan section 11); a hand edit to a
/// projected title is overwritten on the next projection.
async fn s27(ctx: &Ctx) -> Result<()> {
    let tag = tag(27);
    let dentist = file(ctx, draft("personal", "Book the dentist", &tag, W_SUCCEED)).await?;
    let research = file(
        ctx,
        draft("research", "Compare dentists nearby", &tag, W_DOC_ALPHA),
    )
    .await?;
    wait_status(ctx, &dentist, Status::Done).await?;
    wait_status(ctx, &research, Status::Done).await?;
    let mut proposal = draft(
        "proposal",
        "Pre-load caldav for calendar errands",
        &tag,
        "Proposal",
    );
    proposal["subject"] = json!("tool:caldav");
    proposal["review_tier"] = json!("standard");
    proposal["edges"] = json!([{ "kind": "waits_for", "depends_on": dentist }]);
    proposal["inputs_from"] = json!([dentist]);
    let proposal_id = file(ctx, proposal).await?;
    let mut build = draft("capability", "Build a tide table tool", &tag, W_SUCCEED);
    build["capability"] = json!("build");
    file(ctx, build).await?;
    let mut acquire = draft(
        "capability",
        "Acquire the dentist portal password",
        &tag,
        W_SUCCEED,
    );
    acquire["capability"] = json!("acquire");
    acquire["trigger"] = json!({ "kind": "on_credential", "value": "dentist_portal" });
    file(ctx, acquire).await?;
    evaluate(ctx).await?;

    let github = &stand_ins(ctx)?.github;
    let about = |name: &str| -> Vec<Value> {
        let needle = format!("{name} {tag}");
        github
            .issues()
            .into_iter()
            .filter(|issue| issue.to_string().contains(&needle))
            .collect()
    };
    ensure!(
        about("Book the dentist").is_empty(),
        "the personal item reached GitHub"
    );
    ensure!(
        about("Compare dentists nearby").is_empty(),
        "the research item reached GitHub"
    );
    ensure!(
        about("Pre-load caldav for calendar errands").is_empty(),
        "the proposal's own title reached GitHub although it names a personal item"
    );
    let projected = github
        .issues()
        .into_iter()
        .find(|issue| issue.to_string().contains(&proposal_id))
        .ok_or_else(|| anyhow!("the proposal was not projected"))?;
    ensure!(
        labels(&projected).iter().any(|l| l == "rustykrab-redacted"),
        "the proposal's issue is not labelled rustykrab-redacted: {:?}",
        labels(&projected)
    );
    ensure!(
        labels(&projected).iter().any(|l| l == "rustykrab-proposal"),
        "the proposal's issue is not labelled rustykrab-proposal: {:?}",
        labels(&projected)
    );
    ensure!(
        projected.to_string().contains("local:#"),
        "the proposal's personal input is not an opaque local reference"
    );
    ensure!(
        !about("Build a tide table tool").is_empty(),
        "the capability build has no issue"
    );
    ensure!(
        about("Acquire the dentist portal password").is_empty(),
        "the credential acquisition reached GitHub"
    );
    let number = projected["number"].as_u64().unwrap_or_default();
    github.hand_edit(number, "title", json!("edited by hand"))?;
    evaluate(ctx).await?;
    let restored = github
        .issues()
        .into_iter()
        .find(|i| i["number"] == number)
        .unwrap_or_default();
    ensure!(
        restored["title"] == projected["title"],
        "the hand edit survived the next projection: {}",
        restored["title"]
    );
    Ok(())
}

// ── functional capability proof ──────────────────────────────────────

/// Write the `node` the daemon's exec path finds first.
fn node_stand_in(bin: &std::path::Path, body: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(bin)?;
    let path = bin.join("node");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n"))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

/// Functional capability proof on a scripted daemon of its own, with its
/// local worker performing the probe through its own exec boundary. The
/// daemon's exec path finds a `node` first that answers, with exit 0, no
/// version: a binary that is there and does not work.
///
/// - The item reports `node` missing, parks `blocked(needs_tool)` and the
///   ladder files `Build tool: node`.
/// - The build first tries to report a passing `operation_probe` and a
///   `repair_generation` of its own, which the worker's report tool
///   refuses, then claims done without repairing anything. The controller
///   writes generation 1 on the waiting item, asks the local worker (the
///   executor that failed it) to run `node --version`, and the receipt says
///   the postcondition failed: the build is not done, its verification
///   fails, nothing forged is stored, and the item stays parked.
/// - The build's repair replaces the stand-in with one that answers its
///   version: generation 2, a passing receipt, the build closes `done` with
///   its operation proven, and only then does the item resume and finish.
async fn capability_proof(shared: &Ctx) -> Result<()> {
    let path = format!(
        "__E2E_DATA_DIR__/e2e-bin:{}",
        std::env::var("PATH").unwrap_or_default()
    );
    let daemon = OwnDaemon::boot_with(shared, &[("PATH", &path)]).await?;
    let ctx = &daemon.ctx;
    let bin = ctx.data_dir.join("e2e-bin");
    node_stand_in(&bin, "echo 'node stand-in: broken, no version'")?;
    let before = std::fs::read_to_string(bin.join("node"))?;
    let tag = "[e2e-control capability-proof]";
    let x = file(ctx, draft("personal", "Run the node stand-in", tag, W_NODE)).await?;
    wait_status(ctx, &x, Status::Blocked(BlockedReason::NeedsTool)).await?;
    let build = wait_listed(ctx, "kind=capability", "the build of node", |v| {
        title(v) == "Build tool: node"
    })
    .await?;
    let build = id(&build);
    let built = match wait_status(ctx, &build, Status::Done).await {
        Ok(view) => view,
        Err(e) => {
            let trail = trail_text(&event_trail(ctx, &build).await?);
            keep_evidence_file(
                "capability-proof/failed.daemon.log",
                &ctx.data_dir.join("daemon.log"),
            )?;
            for log in evidence_of(&item(ctx, &build).await?, "run_log") {
                if let Some(path) = log["reference"].as_str() {
                    let name = std::path::Path::new(path)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    keep_evidence_file(
                        &format!("capability-proof/failed.{name}"),
                        std::path::Path::new(path),
                    )?;
                }
            }
            return Err(e.context(format!("the build's trail:\n{trail}")));
        }
    };
    let finished = wait_status(ctx, &x, Status::Done).await?;
    let after = std::fs::read_to_string(bin.join("node"))?;
    ensure!(
        after.contains(NODE_VERSION),
        "the repair replaced the stand-in: {after}"
    );

    // ── the receipts: one failed, one passing, both the host's ────────
    let executor =
        worker_of(&finished).ok_or_else(|| anyhow!("the waiting item names no worker"))?;
    let receipts = evidence_of(&built, "operation_probe");
    ensure!(
        receipts.len() == 2,
        "two host receipts on the build, nothing a report forged: {}",
        evidence_text(&built)
    );
    let probes: Vec<Value> = receipts
        .iter()
        .map(|e| serde_json::from_str(e["reference"].as_str().unwrap_or_default()))
        .collect::<std::result::Result<_, _>>()?;
    for (row, probe) in receipts.iter().zip(&probes) {
        ensure!(
            row["verified_by"] == format!("executor:{executor}").as_str()
                && probe["executor"] == executor.as_str()
                && probe["operation"] == "binary.available/node"
                && row["hash"] == probe["inputs_digest"],
            "a receipt from the executor, bound to its inputs: {row}"
        );
    }
    ensure!(
        probes[0]["outcome"] == "failed" && probes[1]["outcome"] == "ok",
        "the first probe failed, the repaired one passed: {probes:?}"
    );
    ensure!(
        probes[0]["generation"] == format!("{x}:1").as_str()
            && probes[1]["generation"] == format!("{x}:2").as_str(),
        "each probe is bound to its own generation of the waiting item: {probes:?}"
    );

    // ── the generations: on the waiting item, naming the probes ───────
    let waiting = item(ctx, &x).await?;
    let generations = evidence_of(&waiting, "repair_generation");
    ensure!(
        generations.len() == 2,
        "two repair generations: {}",
        evidence_text(&waiting)
    );
    for (n, (row, probe)) in generations.iter().zip(&probes).enumerate() {
        let record: Value = serde_json::from_str(row["reference"].as_str().unwrap_or_default())?;
        let planned = &record["probes"][0]["expected"];
        ensure!(
            record["n"] == n as u64 + 1
                && record["cause"] == "capability"
                && record["source_item"] == build.as_str()
                && row["verified_by"] == "controller"
                && [
                    "operation",
                    "executor",
                    "inputs_digest",
                    "generation",
                    "run"
                ]
                .iter()
                .all(|k| planned[*k] == probe[*k]),
            "generation {} records the probe it was judged by: {record} {probe}",
            n + 1
        );
    }

    // ── the build: not done on its word, done on its proof ────────────
    let build_trail = event_trail(ctx, &build).await?;
    let unproven = build_trail.iter().find(|e| {
        moved_to(e) == Some(Status::Blocked(BlockedReason::VerificationFailed))
            && e["reason"]
                .as_str()
                .is_some_and(|r| r.contains("the operation probe failed"))
    });
    ensure!(
        unproven.is_some(),
        "the unrepaired build failed verification on its probe: {}",
        trail_text(&build_trail)
    );
    let closed = build_trail
        .iter()
        .find(|e| moved_to(e) == Some(Status::Done))
        .ok_or_else(|| anyhow!("the build never closed"))?;
    ensure!(
        closed["reason"]
            .as_str()
            .is_some_and(|r| r.contains("operation proven: binary.available/node")),
        "the build closed on its proof: {closed}"
    );
    // ── the forged receipt: refused at the worker's own report tool ────
    let first_log = evidence_of(&built, "run_log")
        .first()
        .and_then(|e| e["reference"].as_str().map(std::path::PathBuf::from))
        .ok_or_else(|| anyhow!("the build's first run left no session log"))?;
    let first_session = std::fs::read_to_string(&first_log)?;
    ensure!(
        first_session.contains("`operation_probe` is not one of")
            && first_session.contains("`repair_generation` is not one of"),
        "the worker's report tool refused the forged kinds: {}",
        excerpt(&first_session)
    );
    ensure!(
        evidence_of(&built, "repair_generation").is_empty()
            && !evidence(&built)
                .iter()
                .any(|e| e["kind"] == "operation_probe"
                    && e["verified_by"] != json!(format!("executor:{executor}"))),
        "nothing forged is on the build: {}",
        evidence_text(&built)
    );

    // ── the waiting item: resumed only on the proof ───────────────────
    let trail = event_trail(ctx, &x).await?;
    let leases = trail.iter().filter(|e| e["kind"] == "lease").count();
    ensure!(
        leases == 2,
        "the item ran twice, before and after: {leases}"
    );
    let revived = trail
        .iter()
        .find(|e| {
            e["reason"].as_str()
                == Some("capability landed and its operation was proven on its executor")
        })
        .ok_or_else(|| anyhow!("the item was not released on the proof"))?;
    let at = |e: &Value| time(&e["at"]).ok_or_else(|| anyhow!("an event without a time: {e}"));
    ensure!(
        at(revived)? >= at(closed)?,
        "released only after the build was proven"
    );
    let second_lease = trail
        .iter()
        .filter(|e| e["kind"] == "lease")
        .nth(1)
        .ok_or_else(|| anyhow!("no second lease"))?;
    ensure!(
        at(second_lease)? >= at(closed)?,
        "no run between the failed probe and the proof"
    );

    // ── the failed verification, as an operator investigates it ───────
    let errors = evidence_of(&built, "error");
    let fingerprint = errors
        .iter()
        .find(|e| {
            e["reference"]
                .as_str()
                .is_some_and(|r| r.contains("the operation probe failed"))
        })
        .and_then(|e| e["hash"].as_str())
        .ok_or_else(|| anyhow!("no error row for the failed probe"))?
        .to_string();
    let printed = cli_stdout(ctx, &["work", "investigate", &fingerprint])?;
    ensure!(
        printed.contains(&build),
        "work investigate names the build: {}",
        excerpt(&printed)
    );

    keep_evidence("capability-proof/node-stand-in.before.sh", &before)?;
    keep_evidence("capability-proof/node-stand-in.after.sh", &after)?;
    keep_evidence("capability-proof/waiting.events.txt", trail_text(&trail))?;
    keep_evidence(
        "capability-proof/waiting.events.json",
        serde_json::to_string_pretty(&trail)?,
    )?;
    keep_evidence(
        "capability-proof/build.events.txt",
        trail_text(&build_trail),
    )?;
    keep_evidence(
        "capability-proof/build.events.json",
        serde_json::to_string_pretty(&build_trail)?,
    )?;
    keep_evidence(
        "capability-proof/build.operation-probe.json",
        serde_json::to_string_pretty(&receipts)?,
    )?;
    keep_evidence(
        "capability-proof/waiting.repair-generation.json",
        serde_json::to_string_pretty(&generations)?,
    )?;
    keep_evidence(
        "capability-proof/build.errors.json",
        serde_json::to_string_pretty(&errors)?,
    )?;
    keep_evidence("capability-proof/work-investigate.txt", &printed)?;
    keep_evidence("capability-proof/build.run-1.session.jsonl", &first_session)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_ids_are_stable_unique_and_carry_their_plan_number() {
        let catalog = catalog();
        for entry in &catalog {
            let number = entry
                .id
                .strip_prefix("control/")
                .and_then(|rest| rest.get(..2))
                .and_then(|n| n.parse::<u8>().ok());
            assert_eq!(number, Some(entry.number), "{}", entry.id);
        }
        let mut ids: Vec<_> = catalog.iter().map(|e| e.id).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count);
    }

    #[test]
    fn every_plan_scenario_appears_exactly_once() {
        let catalog = catalog();
        for number in 1..=32u8 {
            let here = catalog.iter().filter(|e| e.number == number).count();
            let elsewhere = COVERED_ELSEWHERE
                .iter()
                .filter(|(n, _)| *n == number)
                .count();
            assert_eq!(here + elsewhere, 1, "plan scenario {number}");
        }
        assert!(catalog.iter().all(|e| (1..=32).contains(&e.number)));
    }

    /// Section 16's table: which phase's exit turns which scenario green.
    #[test]
    fn scenarios_carry_the_phase_that_turns_them_green() {
        let numbers = |phase: Phase| -> Vec<u8> {
            catalog()
                .iter()
                .filter(|e| e.phase == phase)
                .map(|e| e.number)
                .collect()
        };
        // Section 16's Phase 1 set, plus 4 (see `catalog`).
        assert_eq!(
            numbers(Phase::One),
            vec![1, 4, 6, 9, 11, 14, 19, 20, 21, 22, 23, 24, 25, 26, 28, 30, 32]
        );
        assert_eq!(numbers(Phase::Three), vec![2, 17, 31]);
        assert_eq!(numbers(Phase::ThreeExit), vec![13]);
        assert_eq!(numbers(Phase::Four), vec![3, 5, 18, 29]);
        assert_eq!(numbers(Phase::Five), vec![8]);
        assert_eq!(numbers(Phase::Six), vec![7, 12, 15, 16, 27]);
    }

    /// Mirrors the planning suite's promotion test. What must pass follows
    /// from `PROMOTED` alone, so promoting a phase is one edit, and every
    /// scenario of a phase not yet shipped stays xfail.
    #[test]
    fn promoted_phases_are_must_pass_and_the_rest_remain_xfail() {
        let catalog = catalog();
        let scenarios = scenarios();
        assert_eq!(scenarios.len(), catalog.len() + fleet_scenarios().len());
        assert_eq!(catalog.len(), 31);
        assert!(fleet_scenarios()
            .iter()
            .all(|(expected, _)| *expected == Expected::Pass));
        let held = |e: &Entry| HELD_BACK.iter().any(|(n, _)| *n == e.number);
        for (entry, (expected, (id, _))) in catalog.iter().zip(&scenarios) {
            assert_eq!(*id, entry.id);
            let want = if PROMOTED.contains(&entry.phase) && !held(entry) {
                Expected::Pass
            } else {
                Expected::XFail
            };
            assert_eq!(*expected, want, "{id}");
        }
        let unshipped = catalog
            .iter()
            .filter(|e| !PROMOTED.contains(&e.phase) || held(e))
            .count();
        let xfail = scenarios
            .iter()
            .filter(|(expected, _)| *expected == Expected::XFail)
            .count();
        assert_eq!(xfail, unshipped);
        // A held-back scenario belongs to a promoted phase and says why.
        for (number, why) in HELD_BACK {
            let entry = catalog.iter().find(|e| e.number == *number).unwrap();
            assert!(
                PROMOTED.contains(&entry.phase),
                "{number} is not in a promoted phase"
            );
            assert!(
                !why.trim().is_empty(),
                "{number} is held back without a reason"
            );
        }
    }
}
