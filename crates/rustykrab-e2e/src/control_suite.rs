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
//! `XFail` until then. Phases 1 and 6 have shipped (see [`PROMOTED`] and
//! [`HELD_BACK`]); the routes of Phases 3 to 5 do not exist, so their
//! scenarios fail at their first request, on a status code rather than a
//! panic or a hang.
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
//! a second scripted daemon as the peer.
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
//!   `archived`). Served since Phase 6: `POST /api/work/evaluate` (run the
//!   nightly evaluation now: decisions synced back, metrics, criteria,
//!   proposals, projection), `GET /api/work/metrics` (the section 1.1
//!   metrics of 16, under `metrics`), `capability: build | acquire` on a
//!   capability draft (27), `subject` and `review_tier` on a proposal
//!   draft (12), and the GitHub adapter reading `RUSTYKRAB_GITHUB_API_BASE`,
//!   `RUSTYKRAB_GITHUB_REPO` and `RUSTYKRAB_GITHUB_TOKEN` (`surface.rs`).
//!   Not served yet: `POST /api/workers` with `command` (the Claude Code
//!   executable) or `base_url` and `token` (a peer).

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
    stack_manifest, ClaudeCodeStandIn, FixtureRepo, CLAIM_WRONGLY, KEEPS_ITS_OWN_TRACKER,
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
const PROMOTED: &[Phase] = &[Phase::One, Phase::Six];

/// Scenarios of a promoted phase that stay `XFail`, each with why. An
/// entry here is a known gap in a shipped phase, named rather than hidden;
/// the report still runs it, and it turns the suite red the day it passes,
/// so it leaves this list the same day.
const HELD_BACK: &[(u8, &str)] = &[
    (
        30,
        "a cron firing still runs as a task-queue conversation: moving it onto a work item \
         needs the job's persistent conversation, SKILL.md injection and per-job delivery \
         target carried into the worker run, and a gate that holds local leases while an \
         interactive turn runs (plan 12.1)",
    ),
    // Phase 6: the avoidable-escalation criterion reads questions through
    // `rustykrab_dream::QuestionReader`, which the daemon wires to nothing
    // until Phase 4's `questions` table and `/api/questions` land.
    (
        15,
        "waits for Phase 4: the scenario asks through GET /api/questions and answers through \
         POST /api/questions/{id}/answer, and dreaming reads the surfaced question and its \
         recorded default through QuestionReader, wired to the questions table at merge",
    ),
];

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
/// xpass; its peer half is scenario 8's to prove in Phase 5.
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
    catalog()
        .into_iter()
        .map(|entry| (entry.expected(), (entry.id, entry.run)))
        .collect()
}

// ── Phase 1: work items and the controller skeleton ──────────────────

/// Scenario 1: A personal task is filed, leased to a local worker with its toolset
/// pre-activated, completed, and reported through Telegram with evidence.
async fn s01(ctx: &Ctx) -> Result<()> {
    let progress = EventStream::open(ctx).await?;
    let conversation = telegram_thread(ctx, 101).await?;
    telegram_say(ctx, 101, S01_FILE).await?;
    let filed = wait_titled(ctx, S01_TITLE).await?;
    let task = id(&filed);
    ensure!(
        body(&filed)["kind"] == "personal",
        "the task is filed as personal: {}",
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
    ensure!(
        reports.iter().any(|m| m.contains(S01_EVIDENCE)),
        "the Telegram report carries the evidence: {reports:?}"
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
/// errand, children over the root's budget and a `code` item are each
/// rejected, each rejection an event; two `blocks`-chained steps that add
/// nothing to each other are accepted with a `sequential_split` warning
/// recorded as an event. Driven by a scripted caller until the planner
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
    let shops = vec![
        step("groceries", "Buy the groceries", "personal", 80_000),
        step("paint", "Buy the paint", "personal", 80_000),
    ];
    let over = rejected(plan(ctx, under(shops, vec![])).await?)?;
    ensure!(
        reasons(&over).contains(&RejectionReason::OverBudget),
        "children over the root's budget are rejected with over_budget: {:?}",
        reasons(&over)
    );
    let fix = vec![step("fix", "Fix the errand app", "code", 5_000)];
    let code = rejected(plan(ctx, under(fix, vec![])).await?)?;
    ensure!(
        reasons(&code).contains(&RejectionReason::KindNotAllowed),
        "a code item in a planned graph is rejected with kind_not_allowed: {:?}",
        reasons(&code)
    );
    ensure!(
        children(ctx, &root).await?.is_empty(),
        "nothing from a rejected graph was stored"
    );
    let rejections =
        Value::Array(events_of(&item(ctx, &root).await?, EventKind::Rejection)).to_string();
    for reason in [
        RejectionReason::TooManyItems,
        RejectionReason::OverBudget,
        RejectionReason::KindNotAllowed,
    ] {
        ensure!(
            rejections.contains(reason.as_str()),
            "the {} rejection is an event dreaming can count: {rejections}",
            reason.as_str()
        );
    }

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
        split
            .warnings
            .iter()
            .any(|w| w.check == rustykrab_core::work::WarningCheck::SequentialSplit),
        "a split made only for sequence is accepted with a sequential_split warning: {:?}",
        split.warnings
    );
    let mut warned = events_of(&item(ctx, &parcels).await?, EventKind::Warning);
    for id in split.ids.values() {
        warned.extend(events_of(&item(ctx, id).await?, EventKind::Warning));
    }
    ensure!(
        Value::Array(warned)
            .to_string()
            .contains("sequential_split"),
        "the sequential_split warning is recorded as an event"
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
    let listed: Vec<String> = list(ctx, "").await?.iter().map(id).collect();
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
    register_claude_code(ctx, "pinch-s17", &repo, &stand_in).await?;
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
    let tag = tag(31);
    let repo = FixtureRepo::create()?;
    let stand_in = ClaudeCodeStandIn::create()?;
    register_claude_code(ctx, "pinch-s31", &repo, &stand_in).await?;
    let mut change = code_draft(
        "Add a status helper and track it",
        &tag,
        KEEPS_ITS_OWN_TRACKER,
        &repo,
    );
    change["worker_kind"] = json!("claude_code");
    let task = file(ctx, change).await?;
    wait_status(ctx, &task, Status::Done).await?;
    let discovered_title = "Add a changelog entry [e2e-control s31]";
    let discovered = wait_titled(ctx, discovered_title).await?;
    tokio::time::sleep(QUIET).await;
    let everything = list(ctx, "").await?;
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
    post(
        ctx,
        &format!("{QUESTIONS}/{question_id}/answer"),
        json!({ "answer": ANSWER_FLORIST }),
    )
    .await?;
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

/// Scenario 5: A worker stalls; the progress ledger triggers repair; the second
/// attempt uses the first attempt's evidence.
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
    let deadline = Instant::now() + SETTLE;
    let attempts = loop {
        let attempts = conversations_mentioning(ctx, &tag)?;
        if attempts.len() >= 2 {
            break attempts;
        }
        ensure!(
            Instant::now() < deadline,
            "no second attempt after {SETTLE:?}"
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
                .calls_to("work_plan")
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
    let calls = run.calls_to("work_plan");
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

/// A second daemon, killed when the scenario ends however it ends.
struct PeerDaemon(Option<std::process::Child>);

impl Drop for PeerDaemon {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Scenario 8: A peer node receives `required_tools`, activates them before its
/// first prefill, and returns a structured result.
async fn s08(ctx: &Ctx) -> Result<()> {
    let tag = tag(8);
    let port = crate::pick_free_port()?;
    let base = format!("http://127.0.0.1:{port}");
    post(
        ctx,
        WORKERS,
        json!({ "name": "krabby", "kind": "peer", "base_url": base, "token": crate::AUTH_TOKEN }),
    )
    .await?;
    let dir = tempfile::Builder::new()
        .prefix("rustykrab-e2e-peer-")
        .tempdir()?;
    let mut peer = PeerDaemon(Some(crate::spawn_daemon(&ctx.bin, dir.path(), port, None)?));
    let child = peer
        .0
        .as_mut()
        .ok_or_else(|| anyhow!("the peer daemon is gone"))?;
    crate::wait_for_health(&base, &ctx.client, child).await?;
    let mut task = draft("personal", "Check the harbour calendar", &tag, W_PROBE);
    task["worker_kind"] = json!("peer");
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
    let peer_db = dir.path().join("db").join("store.db");
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
    post(
        ctx,
        &format!("{QUESTIONS}/{question_id}/answer"),
        json!({ "answer": "9am" }),
    )
    .await?;
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
/// as `local:#N`; a hand edit to a projected title is overwritten on the
/// next projection.
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
    proposal["edges"] = json!([{ "kind": "waits_for", "depends_on": dentist }]);
    proposal["inputs_from"] = json!([dentist]);
    file(ctx, proposal).await?;
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
    let projected = about("Pre-load caldav for calendar errands")
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("the proposal was not projected"))?;
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
        restored["title"]
            .as_str()
            .is_some_and(|t| t.contains("Pre-load caldav")),
        "the hand edit survived the next projection: {}",
        restored["title"]
    );
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
        assert_eq!(scenarios.len(), 31);
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
