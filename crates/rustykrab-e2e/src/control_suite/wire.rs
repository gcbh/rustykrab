//! The wire under test and how the control suite reads it: routes,
//! filings, item views, events, the Telegram stand-in, the daemon's store
//! and the SSE stream. Each wire assumption of the suite's module docs is
//! made in one helper here, so reconciling it with the implementation is
//! one edit.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, ensure, Context, Result};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use rustykrab_core::work::{
    ArtifactRef, EdgeKind, EventKind, PlanAccepted, PlanOutcome, PlanRejected, RejectionReason,
    Rung, Status, WorkError,
};

use super::script::*;
use crate::fixture_repo::{ClaudeCodeStandIn, FixtureRepo};
use crate::surface::{StandIns, TG_CHAT_ID, WEBHOOK_SECRET};
use crate::transcript::Transcript;
use crate::Ctx;

// ── the surface under test (plan section 14) ─────────────────────────

pub(super) const WORK: &str = "/api/work";
pub(super) const WORK_PLAN: &str = "/api/work/plan";
pub(super) const WORK_ARCHIVE: &str = "/api/work/archive";
pub(super) const WORK_EVENTS: &str = "/api/work/events";
pub(super) const WORK_IMPORT: &str = "/api/work/import";
pub(super) const WORK_EVALUATE: &str = "/api/work/evaluate";
pub(super) const WORK_METRICS: &str = "/api/work/metrics";
pub(super) const WORKERS: &str = "/api/workers";
pub(super) const QUESTIONS: &str = "/api/questions";

/// How long a scenario waits for the controller to reach a state. Well
/// inside the harness's 120 s per scenario.
pub(super) const SETTLE: Duration = Duration::from_secs(60);
pub(super) const POLL: Duration = Duration::from_millis(250);
/// After the first message about something arrives, how long to keep
/// listening before counting: longer than any coalescing window.
pub(super) const QUIET: Duration = Duration::from_secs(5);

// ── requests ─────────────────────────────────────────────────────────

pub(super) fn excerpt(text: &str) -> String {
    text.chars().take(400).collect()
}

pub(super) async fn expect(response: reqwest::Response, ok: &[u16], what: &str) -> Result<Value> {
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    ensure!(
        ok.contains(&status),
        "{what} returned {status}, want one of {ok:?}: {}",
        excerpt(&text)
    );
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text).with_context(|| {
        format!(
            "{what} answered with something that is not JSON: {}",
            excerpt(&text)
        )
    })
}

pub(super) async fn get(ctx: &Ctx, path: &str) -> Result<Value> {
    expect(ctx.get(path).await?, &[200], &format!("GET {path}")).await
}

pub(super) async fn post(ctx: &Ctx, path: &str, body: Value) -> Result<Value> {
    expect(
        ctx.post(path, body).await?,
        &[200, 201, 202, 204],
        &format!("POST {path}"),
    )
    .await
}

// ── filings ──────────────────────────────────────────────────────────

/// Any filing path: accepted whole or rejected whole, typed either way.
pub(super) async fn filing(ctx: &Ctx, path: &str, body: Value) -> Result<PlanOutcome> {
    let answer = expect(
        ctx.post(path, body).await?,
        &[200, 201, 422],
        &format!("POST {path}"),
    )
    .await?;
    serde_json::from_value(answer.clone())
        .with_context(|| format!("POST {path} did not answer with a filing outcome: {answer}"))
}

/// `POST /api/work/plan`: the `work_plan` contract.
pub(super) async fn plan(ctx: &Ctx, body: Value) -> Result<PlanOutcome> {
    filing(ctx, WORK_PLAN, body).await
}

/// `POST /api/work`: file one item (the REST face of `work_file`).
pub(super) async fn file(ctx: &Ctx, draft: Value) -> Result<String> {
    Ok(accepted(filing(ctx, WORK, draft).await?)?.root)
}

pub(super) fn accepted(outcome: PlanOutcome) -> Result<PlanAccepted> {
    match outcome {
        PlanOutcome::Accepted(accepted) => Ok(accepted),
        PlanOutcome::Rejected(rejected) => bail!("the filing was rejected: {:?}", rejected.failed),
    }
}

pub(super) fn rejected(outcome: PlanOutcome) -> Result<PlanRejected> {
    match outcome {
        PlanOutcome::Rejected(rejected) => Ok(rejected),
        PlanOutcome::Accepted(accepted) => {
            bail!(
                "the filing was accepted (root {}), want it rejected",
                accepted.root
            )
        }
    }
}

pub(super) fn reasons(rejected: &PlanRejected) -> Vec<RejectionReason> {
    rejected.failed.iter().map(|check| check.reason).collect()
}

pub(super) fn id_of(accepted: &PlanAccepted, tmp: &str) -> Result<String> {
    accepted
        .ids
        .get(tmp)
        .cloned()
        .ok_or_else(|| anyhow!("the accepted plan has no id for {tmp}: {:?}", accepted.ids))
}

// ── drafts ───────────────────────────────────────────────────────────

/// A tag unique to one run of one scenario, carried in every title it
/// files, so its items, briefs and messages can be found again.
pub(super) fn tag(number: u8) -> String {
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    format!("[e2e-control s{number:02} {}]", &nonce[..8])
}

pub(super) fn later(seconds: i64) -> String {
    (Utc::now() + chrono::Duration::seconds(seconds)).to_rfc3339()
}

/// `trigger: at(time)`, in `Trigger`'s serde form.
pub(super) fn trigger_at(seconds: i64) -> Value {
    json!({ "kind": "at", "value": later(seconds) })
}

/// A trigger far enough away that the item stays queued for the scenario.
pub(super) fn not_yet() -> Value {
    trigger_at(365 * 24 * 3600)
}

/// One item to file. `worker` is the script its worker replays.
pub(super) fn draft(kind: &str, name: &str, tag: &str, worker: &str) -> Value {
    let title = format!("{name} {tag}");
    json!({
        "kind": kind,
        "title": title,
        "objective": format!("{worker}. {title}"),
        "done_when": format!("the worker reports {name} finished, with evidence"),
    })
}

/// A leaf of a `work_plan` graph, with a budget that fits its parent's.
pub(super) fn node(
    tmp: &str,
    parent: &str,
    kind: &str,
    name: &str,
    tag: &str,
    worker: &str,
) -> Value {
    let mut item = draft(kind, name, tag, worker);
    item["tmp"] = json!(tmp);
    item["parent"] = json!({ "tmp": parent });
    item["budget"] = budget(20_000);
    item
}

/// The root of a `work_plan` graph. A parent is never leased, so it names
/// no worker behaviour. Its budget is an envelope over its children's
/// (plan section 4.2: children's budgets sum within the root's), so it
/// covers ten [`node`]s in every dimension.
pub(super) fn root_node(tmp: &str, name: &str, tag: &str) -> Value {
    json!({
        "tmp": tmp,
        "kind": "personal",
        "title": format!("{name} {tag}"),
        "objective": name,
        "done_when": "every child is closed and the outcome is reported",
        "budget": envelope(200_000),
    })
}

pub(super) fn edge(item: &str, kind: EdgeKind, depends_on: &str) -> Value {
    json!({ "item": { "tmp": item }, "kind": kind.as_str(), "depends_on": { "tmp": depends_on } })
}

/// A `work_plan` body rooted at a temp id, reporting to `origin`'s channel.
pub(super) fn graph(
    root: &str,
    items: Vec<Value>,
    edges: Vec<Value>,
    origin: Option<&str>,
) -> Value {
    let mut body = json!({
        "root": { "tmp": root },
        "items": items,
        "edges": edges,
        "rationale": "e2e-control scenario graph",
    });
    if let Some(conversation) = origin {
        body["origin_conversation_id"] = json!(conversation);
    }
    body
}

/// File a `work_plan` graph rooted at temp id `P` and require acceptance.
pub(super) async fn file_graph(
    ctx: &Ctx,
    items: Vec<Value>,
    edges: Vec<Value>,
    origin: Option<&str>,
) -> Result<PlanAccepted> {
    accepted(plan(ctx, graph("P", items, edges, origin)).await?)
}

/// The real ids of an accepted graph, in the order of `tmps`.
pub(super) fn ids<const N: usize>(accepted: &PlanAccepted, tmps: [&str; N]) -> Result<[String; N]> {
    let mut out: [String; N] = std::array::from_fn(|_| String::new());
    for (slot, tmp) in out.iter_mut().zip(tmps) {
        *slot = id_of(accepted, tmp)?;
    }
    Ok(out)
}

// ── items ────────────────────────────────────────────────────────────

/// The item itself, whether the response is flat or wraps it in `item`.
pub(super) fn body(view: &Value) -> &Value {
    if view["item"].is_object() {
        &view["item"]
    } else {
        view
    }
}

/// A field beside the item, or on it.
pub(super) fn field<'a>(view: &'a Value, key: &str) -> &'a Value {
    if view[key].is_null() {
        &body(view)[key]
    } else {
        &view[key]
    }
}

pub(super) fn id(view: &Value) -> String {
    body(view)["id"].as_str().unwrap_or_default().to_string()
}

pub(super) fn title(view: &Value) -> String {
    body(view)["title"].as_str().unwrap_or_default().to_string()
}

pub(super) fn status(view: &Value) -> Result<Status> {
    let item = body(view);
    match &item["status"] {
        Value::String(name) => Ok(Status::parse(name, item["status_reason"].as_str())),
        object @ Value::Object(_) => serde_json::from_value(object.clone())
            .with_context(|| format!("unreadable status {object}")),
        other => bail!("item {} has no status: {other}", id(view)),
    }
}

pub(super) fn shown(view: &Value) -> String {
    status(view)
        .map(|s| s.to_string())
        .unwrap_or_else(|e| e.to_string())
}

/// The root-cause item of a cascade status.
pub(super) fn origin(view: &Value) -> Option<String> {
    body(view)["status_origin"].as_str().map(str::to_string)
}

pub(super) fn is_active(view: &Value) -> bool {
    status(view).is_ok_and(|s| s.is_active())
}

pub(super) async fn item(ctx: &Ctx, id: &str) -> Result<Value> {
    get(ctx, &format!("{WORK}/{id}")).await
}

/// Poll an item until `ready` holds. A failed read fails at once: an id
/// the daemon handed out must stay readable.
pub(super) async fn wait_for(
    ctx: &Ctx,
    id: &str,
    what: &str,
    ready: impl Fn(&Value) -> bool,
) -> Result<Value> {
    let deadline = Instant::now() + SETTLE;
    loop {
        let view = item(ctx, id).await?;
        if ready(&view) {
            return Ok(view);
        }
        ensure!(
            Instant::now() < deadline,
            "{what}: {} is still {} after {SETTLE:?}",
            title(&view),
            shown(&view)
        );
        tokio::time::sleep(POLL).await;
    }
}

/// Poll an item until it has status `want`; fail early if it closes as
/// anything else, since closed is final.
pub(super) async fn wait_status(ctx: &Ctx, id: &str, want: Status) -> Result<Value> {
    let deadline = Instant::now() + SETTLE;
    loop {
        let view = item(ctx, id).await?;
        let now = status(&view)?;
        if now == want {
            return Ok(view);
        }
        ensure!(
            !now.is_closed(),
            "{} closed as {now}, want {want}",
            title(&view)
        );
        ensure!(
            Instant::now() < deadline,
            "{} is still {now} after {SETTLE:?}, want {want}",
            title(&view)
        );
        tokio::time::sleep(POLL).await;
    }
}

pub(super) async fn expect_status(ctx: &Ctx, id: &str, want: Status, claim: &str) -> Result<Value> {
    let view = item(ctx, id).await?;
    ensure!(
        status(&view)? == want,
        "{claim}: {} is {}, want {want}",
        title(&view),
        shown(&view)
    );
    Ok(view)
}

/// Wait for a cascade status and check it names `origin_id`.
pub(super) async fn wait_cascade(
    ctx: &Ctx,
    id: &str,
    want: Status,
    origin_id: &str,
) -> Result<Value> {
    let view = wait_status(ctx, id, want).await?;
    ensure!(
        origin(&view).as_deref() == Some(origin_id),
        "{} is {want} with origin {:?}, want {origin_id}",
        title(&view),
        origin(&view)
    );
    Ok(view)
}

// ── events, evidence, leases ─────────────────────────────────────────

pub(super) fn events(view: &Value) -> Vec<Value> {
    field(view, "events")
        .as_array()
        .cloned()
        .unwrap_or_default()
}

pub(super) fn events_of(view: &Value, kind: EventKind) -> Vec<Value> {
    events(view)
        .into_iter()
        .filter(|event| event["kind"] == kind.as_str())
        .collect()
}

pub(super) fn time(value: &Value) -> Option<DateTime<Utc>> {
    value
        .as_str()
        .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        .map(|t| t.with_timezone(&Utc))
}

/// The status an event moved the item to.
pub(super) fn moved_to(event: &Value) -> Option<Status> {
    match &event["to"] {
        Value::String(name) => Some(Status::parse(name, event["reason"].as_str())),
        object @ Value::Object(_) => serde_json::from_value(object.clone()).ok(),
        _ => None,
    }
}

pub(super) fn was_leased(view: &Value) -> bool {
    !events_of(view, EventKind::Lease).is_empty()
        || events(view)
            .iter()
            .any(|event| moved_to(event).is_some_and(|s| s.is_active()))
        || !field(view, "lease").is_null()
}

/// When the item was first leased.
pub(super) fn leased_at(view: &Value) -> Result<DateTime<Utc>> {
    events(view)
        .iter()
        .filter(|event| {
            event["kind"] == EventKind::Lease.as_str()
                || moved_to(event).is_some_and(|s| s.is_active())
        })
        .filter_map(|event| time(&event["at"]))
        .min()
        .ok_or_else(|| anyhow!("{} records no lease", title(view)))
}

/// When the item closed.
pub(super) fn closed_at(view: &Value) -> Result<DateTime<Utc>> {
    time(&body(view)["closed_at"])
        .or_else(|| {
            events(view)
                .iter()
                .filter(|event| moved_to(event).is_some_and(|s| s.is_closed()))
                .filter_map(|event| time(&event["at"]))
                .min()
        })
        .ok_or_else(|| anyhow!("{} records no closing time", title(view)))
}

/// From first lease to close: when the item held its worker.
pub(super) fn active_span(view: &Value) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
    Ok((leased_at(view)?, closed_at(view)?))
}

/// Every rung the item climbed, with when, from its ladder and its events.
pub(super) fn rungs(view: &Value) -> Vec<(String, Option<DateTime<Utc>>)> {
    let ladder = field(view, "ladder")
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|rung| Some((rung["rung"].as_str()?.to_string(), time(&rung["at"]))));
    let logged = events_of(view, EventKind::Rung)
        .into_iter()
        .filter_map(|event| {
            let name = match event["rung"].as_str() {
                Some(name) => name.to_string(),
                None => rung_named_by(event["reason"].as_str()?)?,
            };
            Some((name, time(&event["at"])))
        });
    ladder.chain(logged).collect()
}

/// The rung a rung event's reason names: the controller stores the
/// `RungEvent` as JSON; a bare `rung: detail` line is read by its prefix.
fn rung_named_by(reason: &str) -> Option<String> {
    if let Ok(parsed) = serde_json::from_str::<Value>(reason) {
        return parsed["rung"].as_str().map(str::to_string);
    }
    Some(reason.split(':').next()?.trim().to_string())
}

pub(super) fn climbed(view: &Value, rung: Rung) -> bool {
    rungs(view).iter().any(|(name, _)| name == rung.as_str())
}

pub(super) fn evidence(view: &Value) -> Vec<Value> {
    field(view, "evidence")
        .as_array()
        .cloned()
        .unwrap_or_default()
}

pub(super) fn evidence_text(view: &Value) -> String {
    Value::Array(evidence(view)).to_string()
}

/// Evidence kept for an item: attached rows, or refs on its events.
pub(super) fn kept_evidence(view: &Value) -> bool {
    !evidence(view).is_empty() || events(view).iter().any(|e| e["evidence_ref"].is_string())
}

/// The worker that held the item: its lease, its `worker` field, or the
/// actor of a worker event.
pub(super) fn worker_of(view: &Value) -> Option<String> {
    let leases = field(view, "leases")
        .as_array()
        .cloned()
        .unwrap_or_default();
    field(view, "lease")["worker"]
        .as_str()
        .or_else(|| body(view)["worker"].as_str())
        .map(str::to_string)
        .or_else(|| {
            leases
                .last()
                .and_then(|l| l["worker"].as_str())
                .map(str::to_string)
        })
        .or_else(|| {
            events(view).iter().find_map(|event| {
                event["actor"]
                    .as_str()
                    .and_then(|actor| actor.strip_prefix("worker:"))
                    .map(str::to_string)
            })
        })
}

/// The `inputs` block copied into the item's brief at lease time.
pub(super) fn lease_inputs(view: &Value) -> Vec<Value> {
    let current = field(view, "lease")["inputs"].as_array().cloned();
    let recorded = field(view, "leases")
        .as_array()
        .and_then(|leases| leases.last())
        .and_then(|lease| lease["inputs"].as_array().cloned());
    current.or(recorded).unwrap_or_default()
}

/// The typed `artifact_refs` an input carries (its evidence and artifacts).
pub(super) fn input_refs(input: &Value) -> Result<Vec<ArtifactRef>> {
    let mut refs = Vec::new();
    for key in ["artifacts", "evidence"] {
        if !input[key].is_null() {
            let typed: Vec<ArtifactRef> =
                serde_json::from_value(input[key].clone()).with_context(|| {
                    format!("an input's {key} are not typed artifact refs: {input}")
                })?;
            refs.extend(typed);
        }
    }
    Ok(refs)
}

/// `GET /api/work/{id}/graph` as a tree: the reply lists the nodes depth
/// first, each with its item and edges; this nests them by parent, each
/// node gaining `children`.
pub(super) async fn graph_tree(ctx: &Ctx, root: &str) -> Result<Value> {
    let reply = get(ctx, &format!("{WORK}/{root}/graph")).await?;
    let nodes = reply["nodes"]
        .as_array()
        .cloned()
        .ok_or_else(|| anyhow!("the graph of {root} has no nodes: {reply}"))?;
    fn nest(nodes: &[Value], id: &str) -> Value {
        let mut node = nodes
            .iter()
            .find(|n| body(n)["id"] == id)
            .cloned()
            .unwrap_or(Value::Null);
        let children: Vec<Value> = nodes
            .iter()
            .filter(|n| body(n)["parent"] == id)
            .map(|n| nest(nodes, body(n)["id"].as_str().unwrap_or_default()))
            .collect();
        node["children"] = Value::Array(children);
        node
    }
    let tree = nest(&nodes, root);
    ensure!(
        !tree["item"].is_null(),
        "the graph of {root} omits its root"
    );
    Ok(tree)
}

pub(super) fn edges(view: &Value) -> Vec<Value> {
    field(view, "edges").as_array().cloned().unwrap_or_default()
}

pub(super) fn has_edge(view: &Value, kind: EdgeKind, depends_on: &str) -> bool {
    edges(view)
        .iter()
        .any(|e| e["kind"] == kind.as_str() && e["depends_on"] == depends_on)
}

// ── lists, workers, questions ────────────────────────────────────────

pub(super) async fn list(ctx: &Ctx, query: &str) -> Result<Vec<Value>> {
    let path = if query.is_empty() {
        WORK.to_string()
    } else {
        format!("{WORK}?{query}")
    };
    let answer = get(ctx, &path).await?;
    let items = if answer.is_array() {
        &answer
    } else {
        &answer["items"]
    };
    items
        .as_array()
        .cloned()
        .ok_or_else(|| anyhow!("GET {path} is not a list: {}", excerpt(&answer.to_string())))
}

/// The rows of `GET /api/work/archive`: an array, or under `archived` (the
/// gateway's `ArchiveList`) or `items`.
pub(super) fn archive_rows(answer: &Value) -> Vec<Value> {
    if let Some(rows) = answer.as_array() {
        return rows.clone();
    }
    answer["archived"]
        .as_array()
        .or_else(|| answer["items"].as_array())
        .cloned()
        .unwrap_or_default()
}

pub(super) async fn find_titled(ctx: &Ctx, wanted: &str) -> Result<Option<Value>> {
    Ok(list(ctx, "")
        .await?
        .into_iter()
        .find(|view| title(view) == wanted))
}

pub(super) async fn wait_titled(ctx: &Ctx, wanted: &str) -> Result<Value> {
    let deadline = Instant::now() + SETTLE;
    loop {
        if let Some(view) = find_titled(ctx, wanted).await? {
            return item(ctx, &id(&view)).await;
        }
        ensure!(
            Instant::now() < deadline,
            "no item titled {wanted:?} after {SETTLE:?}"
        );
        tokio::time::sleep(POLL).await;
    }
}

/// Poll `GET /api/work?{query}` until an item, read in full, satisfies
/// `wanted`.
pub(super) async fn wait_listed(
    ctx: &Ctx,
    query: &str,
    what: &str,
    wanted: impl Fn(&Value) -> bool,
) -> Result<Value> {
    let deadline = Instant::now() + SETTLE;
    loop {
        for listed in list(ctx, query).await? {
            let view = item(ctx, &id(&listed)).await?;
            if wanted(&view) {
                return Ok(view);
            }
        }
        ensure!(Instant::now() < deadline, "{what}: none after {SETTLE:?}");
        tokio::time::sleep(POLL).await;
    }
}

/// Every live child of `parent`, closed ones included, sorted: what a
/// re-plan would change.
pub(super) async fn children(ctx: &Ctx, parent: &str) -> Result<Vec<String>> {
    let mut ids: Vec<String> = list(ctx, &format!("parent={parent}&include_closed=true"))
        .await?
        .iter()
        .map(id)
        .collect();
    ids.sort();
    Ok(ids)
}

pub(super) async fn workers(ctx: &Ctx) -> Result<Vec<Value>> {
    let answer = get(ctx, WORKERS).await?;
    let all = if answer.is_array() {
        &answer
    } else {
        &answer["workers"]
    };
    all.as_array()
        .cloned()
        .ok_or_else(|| anyhow!("GET {WORKERS} is not a list: {answer}"))
}

pub(super) async fn questions(ctx: &Ctx) -> Result<Vec<Value>> {
    let answer = get(ctx, QUESTIONS).await?;
    let all = if answer.is_array() {
        &answer
    } else {
        &answer["questions"]
    };
    all.as_array()
        .cloned()
        .ok_or_else(|| anyhow!("GET {QUESTIONS} is not a list: {answer}"))
}

pub(super) async fn questions_for(ctx: &Ctx, item_id: &str) -> Result<Vec<Value>> {
    Ok(questions(ctx)
        .await?
        .into_iter()
        .filter(|q| q["item"] == item_id)
        .collect())
}

pub(super) async fn wait_question(ctx: &Ctx, item_id: &str) -> Result<Value> {
    let deadline = Instant::now() + SETTLE;
    loop {
        if let Some(question) = questions_for(ctx, item_id).await?.into_iter().next() {
            return Ok(question);
        }
        ensure!(
            Instant::now() < deadline,
            "no question for item {item_id} after {SETTLE:?}"
        );
        tokio::time::sleep(POLL).await;
    }
}

/// Run the nightly evaluation now (Phase 6): metrics, criteria, proposals,
/// projection and back-sync.
pub(super) async fn evaluate(ctx: &Ctx) -> Result<Value> {
    post(ctx, WORK_EVALUATE, json!({})).await
}

pub(super) async fn proposals_mentioning(ctx: &Ctx, needle: &str) -> Result<Vec<Value>> {
    let mut found = Vec::new();
    for listed in list(ctx, "kind=proposal").await? {
        let view = item(ctx, &id(&listed)).await?;
        if view.to_string().contains(needle) {
            found.push(view);
        }
    }
    Ok(found)
}

pub(super) async fn register_claude_code(
    ctx: &Ctx,
    name: &str,
    repo: &FixtureRepo,
    stand_in: &ClaudeCodeStandIn,
) -> Result<()> {
    post(
        ctx,
        WORKERS,
        json!({
            "name": name,
            "kind": "claude_code",
            "repos": [repo.path()],
            "command": stand_in.path(),
        }),
    )
    .await?;
    Ok(())
}

/// A `code` draft on the fixture repository.
pub(super) fn code_draft(name: &str, tag: &str, worker: &str, repo: &FixtureRepo) -> Value {
    let mut item = draft("code", name, tag, worker);
    item["artifact_refs"] = json!([{ "kind": "path", "value": repo.path() }]);
    item["writable_resources"] = json!([format!("repo:{}", repo.path().display())]);
    item
}

// ── the Telegram stand-in ────────────────────────────────────────────

pub(super) fn stand_ins(ctx: &Ctx) -> Result<&StandIns> {
    ctx.stand_ins
        .as_ref()
        .ok_or_else(|| anyhow!("this daemon was booted without the Telegram and GitHub stand-ins"))
}

static UPDATE_ID: AtomicI64 = AtomicI64::new(70_000);

/// Send `text` as the user, in forum thread `thread` of the allowed chat.
pub(super) async fn telegram_say(ctx: &Ctx, thread: i64, text: &str) -> Result<()> {
    let update = UPDATE_ID.fetch_add(1, Ordering::SeqCst);
    let response = ctx
        .client
        .post(ctx.url("/webhook/telegram"))
        .header("x-telegram-bot-api-secret-token", WEBHOOK_SECRET)
        .json(&json!({
            "update_id": update,
            "message": {
                "message_id": update,
                "date": Utc::now().timestamp(),
                "chat": { "id": TG_CHAT_ID, "type": "private" },
                "from": { "id": 7, "first_name": "E2E" },
                "message_thread_id": thread,
                "text": text,
            },
        }))
        .send()
        .await?;
    ensure!(
        response.status().is_success(),
        "the Telegram webhook returned {}",
        response.status()
    );
    Ok(())
}

/// Open a Telegram thread and return the conversation the daemon bound to
/// it, so items can name it as their origin.
pub(super) async fn telegram_thread(ctx: &Ctx, thread: i64) -> Result<String> {
    telegram_say(ctx, thread, &format!("e2e-control: open thread {thread}")).await?;
    let key = format!("{TG_CHAT_ID}:{thread}");
    let deadline = Instant::now() + SETTLE;
    loop {
        let bound = strings(
            ctx,
            "SELECT conv_id FROM channel_bindings WHERE channel = 'telegram' AND external_key = ?1",
            &key,
        )?;
        if let Some(conversation) = bound.into_iter().next() {
            return Ok(conversation);
        }
        ensure!(
            Instant::now() < deadline,
            "Telegram thread {key} was never bound to a conversation"
        );
        tokio::time::sleep(POLL).await;
    }
}

/// Messages the bot has sent that contain `needle`.
pub(super) fn told(ctx: &Ctx, needle: &str) -> Result<Vec<String>> {
    Ok(stand_ins(ctx)?
        .telegram
        .all()
        .into_iter()
        .filter(|message| message.contains(needle))
        .collect())
}

pub(super) async fn wait_told(ctx: &Ctx, needle: &str) -> Result<Vec<String>> {
    let deadline = Instant::now() + SETTLE;
    loop {
        let messages = told(ctx, needle)?;
        if !messages.is_empty() {
            return Ok(messages);
        }
        ensure!(
            Instant::now() < deadline,
            "no Telegram message contained {needle:?} after {SETTLE:?}"
        );
        tokio::time::sleep(POLL).await;
    }
}

/// The one message containing `needle`: wait for it, keep listening past
/// any coalescing window, and fail on a second.
pub(super) async fn told_once(ctx: &Ctx, needle: &str) -> Result<String> {
    wait_told(ctx, needle).await?;
    tokio::time::sleep(QUIET).await;
    let mut messages = told(ctx, needle)?;
    ensure!(
        messages.len() == 1,
        "want one Telegram message containing {needle:?}, got {}: {messages:?}",
        messages.len()
    );
    Ok(messages.remove(0))
}

// ── the daemon's store ───────────────────────────────────────────────

pub(super) fn read_only(ctx: &Ctx) -> Result<rusqlite::Connection> {
    Ok(rusqlite::Connection::open_with_flags(
        &ctx.db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?)
}

/// The first column of every row, as text.
pub(super) fn strings(ctx: &Ctx, sql: &str, param: &str) -> Result<Vec<String>> {
    let conn = read_only(ctx)?;
    let mut statement = conn.prepare(sql)?;
    let rows = statement.query_map([param], |row| row.get::<_, Option<String>>(0))?;
    let mut out = Vec::new();
    for row in rows {
        if let Some(value) = row? {
            out.push(value);
        }
    }
    Ok(out)
}

/// Conversations whose stored messages contain `marker`, oldest first.
pub(super) fn conversations_mentioning(ctx: &Ctx, marker: &str) -> Result<Vec<String>> {
    strings(
        ctx,
        "SELECT conversation_id FROM messages WHERE instr(data, ?1) > 0
          GROUP BY conversation_id ORDER BY MIN(rowid)",
        marker,
    )
}

/// Worker runs for an item: conversations that mention `marker` (carried
/// in the item's objective, so in its brief) and called `result_report`.
pub(super) fn worker_runs(ctx: &Ctx, marker: &str) -> Result<Vec<Transcript>> {
    let mut runs = Vec::new();
    for conversation in conversations_mentioning(ctx, marker)? {
        let transcript = Transcript::from_store(&ctx.db_path, &conversation)?;
        if !transcript.calls_to("result_report").is_empty() {
            runs.push(transcript);
        }
    }
    Ok(runs)
}

/// The tool set a worker had active at its first step, from the
/// [`probe_active_tools`] call its script opens with.
pub(super) fn active_at_first_step(run: &Transcript) -> Vec<String> {
    run.calls_to("tools_load")
        .first()
        .and_then(|probe| probe.output.as_ref())
        .and_then(|output| output["active"].as_array())
        .map(|names| {
            names
                .iter()
                .filter_map(|n| n.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

pub(super) fn open_store(ctx: &Ctx) -> Result<rustykrab_store::Store> {
    Ok(rustykrab_store::Store::open(
        ctx.data_dir.join("db"),
        crate::hex_decode(crate::MASTER_KEY_HEX)?,
    )?)
}

// ── the SSE progress stream ──────────────────────────────────────────

/// Frames read from `GET /api/work/events` in the background, until
/// dropped.
pub(super) struct EventStream {
    frames: Arc<Mutex<Vec<(String, Value)>>>,
    reader: tokio::task::JoinHandle<()>,
}

impl Drop for EventStream {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl EventStream {
    pub(super) async fn open(ctx: &Ctx) -> Result<Self> {
        use tokio_stream::StreamExt;

        let response = ctx.get(WORK_EVENTS).await?;
        ensure!(
            response.status() == 200,
            "GET {WORK_EVENTS} returned {}",
            response.status()
        );
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        ensure!(
            content_type.starts_with("text/event-stream"),
            "GET {WORK_EVENTS} is not an event stream: {content_type}"
        );
        let frames = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&frames);
        let reader = tokio::spawn(async move {
            let mut buffer = String::new();
            let mut stream = response.bytes_stream();
            while let Some(Ok(chunk)) = stream.next().await {
                buffer.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(end) = buffer.find("\n\n") {
                    let frame: String = buffer.drain(..end + 2).collect();
                    let mut kind = String::new();
                    let mut data = String::new();
                    for line in frame.lines() {
                        if let Some(v) = line.strip_prefix("event:") {
                            kind = v.trim().to_string();
                        } else if let Some(v) = line.strip_prefix("data:") {
                            data.push_str(v.trim());
                        }
                    }
                    let data = serde_json::from_str(&data).unwrap_or(Value::String(data));
                    sink.lock().unwrap().push((kind, data));
                }
            }
        });
        Ok(Self { frames, reader })
    }

    /// Frames about one item.
    pub(super) fn about(&self, item_id: &str) -> Vec<(String, Value)> {
        self.frames
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, data)| data["item"] == item_id || body(data)["id"] == item_id)
            .cloned()
            .collect()
    }
}

// ── more helpers the scenarios share ─────────────────────────────────

/// Wait for an item's classified error.
pub(super) async fn wait_error(ctx: &Ctx, id: &str) -> Result<WorkError> {
    let view = wait_for(ctx, id, "a classified error", |v| {
        !field(v, "last_error").is_null()
    })
    .await?;
    serde_json::from_value(field(&view, "last_error").clone())
        .with_context(|| format!("unreadable last_error on {}", title(&view)))
}

/// Whether a user-facing message names a rung: by name, in words, or by
/// its order in the ladder.
pub(super) fn mentions_rung(message: &str, rung: &str) -> bool {
    let lower = message.to_lowercase();
    let order = serde_json::from_value::<Rung>(json!(rung))
        .ok()
        .map(|r| format!("order {}", r.order()));
    lower.contains(rung)
        || lower.contains(&rung.replace('_', " "))
        || order.is_some_and(|o| lower.contains(&o))
}

/// The status and origin of each item, for comparing across a restart.
pub(super) async fn snapshot(ctx: &Ctx, ids: &[&String]) -> Result<Vec<(String, Option<String>)>> {
    let mut out = Vec::new();
    for id in ids {
        let view = item(ctx, id).await?;
        out.push((shown(&view), origin(&view)));
    }
    Ok(out)
}

/// The work item a scheduled job's firing became.
pub(super) async fn wait_job_item(ctx: &Ctx, job: &str) -> Result<String> {
    let deadline = Instant::now() + SETTLE;
    loop {
        let fired = strings(
            ctx,
            "SELECT work_item_id FROM scheduled_jobs WHERE id = ?1",
            job,
        )?;
        if let Some(item) = fired.into_iter().next() {
            return Ok(item);
        }
        ensure!(
            Instant::now() < deadline,
            "scheduled job {job} never became a work item"
        );
        tokio::time::sleep(POLL).await;
    }
}

pub(super) fn cli_stdout(ctx: &Ctx, args: &[&str]) -> Result<String> {
    let output = ctx.cli(args)?;
    ensure!(
        output.status.success(),
        "rustykrab {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `key=value` pairs as a URL query string.
pub(super) fn query(pairs: &[(&str, &str)]) -> Result<String> {
    let mut url = reqwest::Url::parse("http://e2e.invalid/")?;
    url.query_pairs_mut().extend_pairs(pairs);
    Ok(url.query().unwrap_or_default().to_string())
}

/// File one queued item and cancel it: a closed item, through REST.
pub(super) async fn file_and_cancel(
    client: reqwest::Client,
    base: String,
    tag: String,
    index: usize,
) -> Result<String> {
    let mut errand = draft(
        "personal",
        &format!("Closed errand {index}"),
        &tag,
        W_SUCCEED,
    );
    errand["trigger"] = not_yet();
    let filed = client
        .post(format!("{base}{WORK}"))
        .bearer_auth(crate::AUTH_TOKEN)
        .json(&errand)
        .send()
        .await?;
    let filed: PlanOutcome =
        serde_json::from_value(expect(filed, &[200, 201], &format!("POST {WORK}")).await?)?;
    let id = accepted(filed)?.root;
    let cancelled = client
        .post(format!("{base}{WORK}/{id}/cancel"))
        .bearer_auth(crate::AUTH_TOKEN)
        .json(&json!({}))
        .send()
        .await?;
    expect(cancelled, &[200, 201, 202, 204], "cancel").await?;
    Ok(id)
}

/// `count` closed items, in filing order. The first goes alone, so a
/// missing route fails on one request rather than hundreds.
pub(super) async fn seed_closed_items(ctx: &Ctx, tag: &str, count: usize) -> Result<Vec<String>> {
    let mut ids =
        vec![file_and_cancel(ctx.client.clone(), ctx.base.clone(), tag.to_string(), 0).await?];
    for start in (1..count).step_by(25) {
        let mut batch = tokio::task::JoinSet::new();
        for index in start..(start + 25).min(count) {
            let (client, base, tag) = (ctx.client.clone(), ctx.base.clone(), tag.to_string());
            batch.spawn(async move { (index, file_and_cancel(client, base, tag, index).await) });
        }
        let mut filed = Vec::new();
        while let Some(joined) = batch.join_next().await {
            let (index, id) = joined?;
            filed.push((index, id?));
        }
        filed.sort();
        ids.extend(filed.into_iter().map(|(_, id)| id));
    }
    Ok(ids)
}

/// Move items' closing time back by `days`, in whatever form the store
/// keeps it. The aging window is measured in days; the harness cannot wait
/// it out, so it ages the rows instead, as the payment suite records a
/// press it cannot make.
pub(super) fn backdate(ctx: &Ctx, ids: &[String], days: i64) -> Result<()> {
    use rusqlite::types::Value as Sql;
    let mut conn = rusqlite::Connection::open(&ctx.db_path)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    let tx = conn.transaction()?;
    for id in ids {
        let current: Sql = tx.query_row(
            "SELECT closed_at FROM work_items WHERE id = ?1",
            [id],
            |row| row.get(0),
        )?;
        let older = match current {
            Sql::Integer(t) if t > 100_000_000_000 => Sql::Integer(t - days * 86_400_000),
            Sql::Integer(t) => Sql::Integer(t - days * 86_400),
            Sql::Text(t) => Sql::Text(shift_text_time(&t, days)?),
            other => bail!("closed_at of {id} is {other:?}, want a time"),
        };
        tx.execute(
            "UPDATE work_items SET closed_at = ?1 WHERE id = ?2",
            rusqlite::params![older, id],
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub(super) fn shift_text_time(raw: &str, days: i64) -> Result<String> {
    if let Ok(t) = DateTime::parse_from_rfc3339(raw) {
        return Ok((t - chrono::Duration::days(days)).to_rfc3339());
    }
    let naive = chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S")
        .with_context(|| format!("unreadable closed_at {raw:?}"))?;
    Ok((naive - chrono::Duration::days(days))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture_repo::stack_manifest;
    use rustykrab_core::work::{
        BlockedReason, CancelReason, ItemRef, Trigger, WorkItemDraft, WorkPlan,
    };

    #[test]
    fn filings_built_here_parse_as_the_shared_vocabulary() {
        let leaf: WorkItemDraft =
            serde_json::from_value(node("a", "P", "personal", "Leaf", "[t]", W_SUCCEED)).unwrap();
        assert_eq!(leaf.parent, Some(ItemRef::Tmp { tmp: "P".into() }));
        let body = graph(
            "P",
            vec![
                root_node("P", "Root", "[t]"),
                node("a", "P", "personal", "Leaf", "[t]", W_SUCCEED),
            ],
            vec![edge("a", EdgeKind::WaitsFor, "P")],
            Some("conversation"),
        );
        let parsed: WorkPlan = serde_json::from_value(body).unwrap();
        assert_eq!(parsed.items.len(), 2);
        assert_eq!(parsed.edges[0].kind, EdgeKind::WaitsFor);
        assert!(matches!(
            serde_json::from_value::<Trigger>(trigger_at(5)).unwrap(),
            Trigger::At(_)
        ));
        let manifest = stack_manifest("slice", false);
        assert_eq!(manifest["layers"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn status_reads_both_the_serde_form_and_the_store_columns() {
        let serde_form = json!({ "item": {
            "id": "x",
            "status": { "status": "blocked", "reason": "upstream_failed" },
            "status_origin": "b",
        } });
        assert_eq!(
            status(&serde_form).unwrap(),
            Status::Blocked(BlockedReason::UpstreamFailed)
        );
        assert_eq!(origin(&serde_form).as_deref(), Some("b"));
        let columns = json!({ "id": "x", "status": "cancelled", "status_reason": "cascade" });
        assert_eq!(
            status(&columns).unwrap(),
            Status::Cancelled(CancelReason::Cascade)
        );
        assert!(status(&json!({ "id": "x" })).is_err());
    }

    #[test]
    fn spans_rungs_and_workers_come_from_the_event_log() {
        let view = json!({
            "id": "x",
            "title": "t",
            "events": [
                { "kind": "transition", "at": "2026-09-26T10:00:00Z", "to": { "status": "queued" }, "actor": "controller" },
                { "kind": "lease", "at": "2026-09-26T10:00:05Z", "actor": "worker:pinch" },
                { "kind": "rung", "at": "2026-09-26T10:00:07Z", "reason": "retry", "actor": "controller" },
                { "kind": "rung", "at": "2026-09-26T10:00:08Z", "actor": "controller",
                  "reason": "{\"rung\":\"switch_worker\",\"at\":\"2026-09-26T10:00:08Z\",\"error\":null,\"outcome\":\"skipped\"}" },
                { "kind": "transition", "at": "2026-09-26T10:00:09Z", "to": { "status": "done" }, "actor": "controller" },
            ],
        });
        let (start, end) = active_span(&view).unwrap();
        assert_eq!((end - start).num_seconds(), 4);
        assert!(climbed(&view, Rung::Retry));
        assert!(climbed(&view, Rung::SwitchWorker));
        assert!(!climbed(&view, Rung::Repair));
        assert_eq!(worker_of(&view).as_deref(), Some("pinch"));
        assert!(was_leased(&view));
        assert!(mentions_rung("stopped at switch worker", "switch_worker"));
        assert!(mentions_rung("it reached order 1", "repair"));
    }
}
