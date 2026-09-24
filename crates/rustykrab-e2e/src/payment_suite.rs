//! The payment approval round trip, through the real daemon.
//!
//! A scripted agent reaches a "checkout" and files a payment request; the
//! harness then plays the user: it opens the one-time `/p/` page the way a
//! phone behind `tailscale serve` would, approves with a public test card,
//! and checks what the plan promises — the terms on the page, the card
//! nowhere on disk, the link dead after use, and the stalled conversation
//! resumed. Declining is checked separately: it records the answer and
//! resumes nothing.
//!
//! The browser half (entering the card and pressing pay) needs Chrome and
//! is covered by the tools crate's live test, not here.

use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

use crate::{hex_decode, Ctx, Expected, ScenarioFn, MASTER_KEY_HEX};

/// The identity `tailscale serve` would inject.
const TAILNET_USER: &str = "e2e-owner@example.com";
/// A public test number. Never a real card.
const TEST_CARD: &str = "4242424242424242";
/// Distinctive enough to find by scanning database bytes.
const HOLDER: &str = "Ada E2eholder Lovelace";

pub(crate) fn scenarios() -> Vec<(Expected, (&'static str, ScenarioFn))> {
    vec![
        (
            Expected::Pass,
            ("payment_approval_resumes_the_turn", |ctx| {
                Box::pin(payment_approval_resumes_the_turn(ctx))
            }),
        ),
        (
            Expected::Pass,
            ("payment_decline_is_recorded", |ctx| {
                Box::pin(payment_decline_is_recorded(ctx))
            }),
        ),
        (
            Expected::Pass,
            ("payment_duplicate_is_held_and_user_alerted", |ctx| {
                Box::pin(payment_duplicate_is_held_and_user_alerted(ctx))
            }),
        ),
    ]
}

/// Start a conversation, send `trigger`, and return the conversation id and
/// the one payment request it filed — checking the terms and the status the
/// scenario expects it to have been filed with.
///
/// Every scenario here shares one daemon and one database, so each takes a
/// trigger of its own rather than all filing the same terms: since the
/// duplicate hold landed, two scenarios asking to pay the same site the
/// same amount would have the second one held, and the test would be
/// measuring the scenarios' interference rather than the daemon.
async fn file_payment(
    ctx: &Ctx,
    trigger: &str,
    minor: i64,
    expect_status: &str,
) -> Result<(String, String)> {
    let conv: Value = ctx
        .post("/api/conversations", json!({}))
        .await?
        .json()
        .await?;
    let conv_id = conv["id"]
        .as_str()
        .ok_or_else(|| anyhow!("conversation create returned no id: {conv}"))?
        .to_string();
    let resp = ctx
        .post(
            &format!("/api/conversations/{conv_id}/messages"),
            json!({"content": trigger}),
        )
        .await?;
    if resp.status() != 200 {
        bail!("send_message returned {}", resp.status());
    }

    let conn = rusqlite::Connection::open_with_flags(
        &ctx.db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let rows: Vec<(String, String, String, i64, String)> = conn
        .prepare(
            "SELECT id, status, origin, amount_minor, currency FROM payment_requests
              WHERE conversation_id = ?1",
        )?
        .query_map([&conv_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<std::result::Result<_, _>>()?;
    let [(id, status, origin, amount, currency)] = rows.as_slice() else {
        bail!("want exactly one payment request for the conversation, got {rows:?}");
    };
    if status != expect_status
        || origin != "http://localhost:9"
        || *amount != minor
        || currency != "USD"
    {
        bail!("payment request recorded the wrong terms: {rows:?}");
    }
    Ok((conv_id, id.clone()))
}

/// Mint a fresh link for the request. The daemon minted none (the harness
/// sets no public URL) and a link's token is never recoverable from the
/// store, so the harness issues its own — exactly what a re-send would do.
async fn issue_link(ctx: &Ctx, request_id: &str) -> Result<String> {
    let store = rustykrab_store::Store::open(ctx.data_dir.join("db"), hex_decode(MASTER_KEY_HEX)?)?;
    Ok(store
        .payment_requests()
        .issue_link(request_id, Duration::from_secs(900))
        .await?)
}

async fn open_page(ctx: &Ctx, token: &str, as_tailnet_user: bool) -> Result<reqwest::Response> {
    let mut req = ctx.client.get(ctx.url(&format!("/p/{token}")));
    if as_tailnet_user {
        req = req.header("Tailscale-User-Login", TAILNET_USER);
    }
    Ok(req.send().await?)
}

async fn submit(ctx: &Ctx, token: &str, form: &[(&str, &str)]) -> Result<reqwest::Response> {
    Ok(ctx
        .client
        .post(ctx.url(&format!("/p/{token}")))
        .header("Tailscale-User-Login", TAILNET_USER)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form_body(form))
        .send()
        .await?)
}

/// `application/x-www-form-urlencoded`, as a browser would post the page.
/// Hand-rolled because the workspace's reqwest is built without `form`.
fn form_body(pairs: &[(&str, &str)]) -> String {
    let encode = |s: &str| -> String {
        s.bytes()
            .map(|b| match b {
                b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (b as char).to_string()
                }
                b' ' => "+".to_string(),
                other => format!("%{other:02X}"),
            })
            .collect()
    };
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn status_of(ctx: &Ctx, request_id: &str) -> Result<(String, Option<String>, Option<String>)> {
    let conn = rusqlite::Connection::open_with_flags(
        &ctx.db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    Ok(conn.query_row(
        "SELECT status, card_last4, decided_by FROM payment_requests WHERE id = ?1",
        [request_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?)
}

async fn payment_approval_resumes_the_turn(ctx: &Ctx) -> Result<()> {
    let (conv_id, request_id) =
        file_payment(ctx, "e2e: pay for the ferry", 4600, "pending").await?;
    let token = issue_link(ctx, &request_id).await?;

    // The page is not an oracle, and not open to anyone off the tailnet.
    if open_page(ctx, &"0".repeat(64), true).await?.status() != 404 {
        bail!("an unknown token did not get the refusal page");
    }
    if open_page(ctx, &token, false).await?.status() != 404 {
        bail!("the page opened without a tailnet identity");
    }

    let page = open_page(ctx, &token, true).await?;
    if page.status() != 200 {
        bail!("approval page returned {}", page.status());
    }
    if page
        .headers()
        .get("cache-control")
        .and_then(|v| v.to_str().ok())
        != Some("no-store")
    {
        bail!("approval page is cacheable");
    }
    let html = page.text().await?;
    for term in ["USD 46.00", "E2E Ferry", "http://localhost:9", "cc-number"] {
        if !html.contains(term) {
            bail!("approval page is missing {term:?}");
        }
    }

    let bad = submit(
        ctx,
        &token,
        &[
            ("decision", "approve"),
            ("holder", HOLDER),
            ("number", "4242424242424241"),
            ("expiry", "12/40"),
            ("security_code", "987"),
        ],
    )
    .await?
    .text()
    .await?;
    if !bad.contains("look right") || bad.contains("4242424242424241") {
        bail!("an invalid card was not refused cleanly, or was echoed back");
    }

    let approved = submit(
        ctx,
        &token,
        &[
            ("decision", "approve"),
            ("holder", HOLDER),
            ("number", TEST_CARD),
            ("expiry", "12/40"),
            ("security_code", "987"),
            ("postal_code", "02554"),
        ],
    )
    .await?;
    if approved.status() != 200 || !approved.text().await?.contains("Approved") {
        bail!("approving did not succeed");
    }

    let (status, last4, by) = status_of(ctx, &request_id)?;
    if status != "authorized"
        || last4.as_deref() != Some("4242")
        || by.as_deref() != Some(TAILNET_USER)
    {
        bail!("approval recorded wrongly: {status} {last4:?} {by:?}");
    }
    if open_page(ctx, &token, true).await?.status() != 404 {
        bail!("the link still works after it was answered");
    }

    let mut on_disk = Vec::new();
    for entry in std::fs::read_dir(ctx.data_dir.join("db"))? {
        let path = entry?.path();
        if path.is_file() {
            on_disk.extend(std::fs::read(path)?);
        }
    }
    let on_disk = String::from_utf8_lossy(&on_disk);
    for leaked in [TEST_CARD, HOLDER, "02554"] {
        if on_disk.contains(leaked) {
            bail!("{leaked:?} was written to the daemon's database files");
        }
    }

    // The stalled conversation resumes on its own.
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let messages: Value = ctx
            .get(&format!("/api/conversations/{conv_id}/messages"))
            .await?
            .json()
            .await?;
        let resumed = messages.as_array().is_some_and(|all| {
            all.iter().any(|m| {
                m["role"] == "assistant"
                    && m["content"]
                        .as_str()
                        .is_some_and(|c| c.contains("resumed after payment approval"))
            })
        });
        let prompted = messages.as_array().is_some_and(|all| {
            all.iter().any(|m| {
                m["role"] == "user"
                    && m["content"]
                        .as_str()
                        .is_some_and(|c| c.contains("fill_payment") && !c.contains(TEST_CARD))
            })
        });
        if resumed && prompted {
            return Ok(());
        }
        if std::time::Instant::now() > deadline {
            bail!("the conversation was not resumed after approval: {messages}");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn payment_decline_is_recorded(ctx: &Ctx) -> Result<()> {
    let (_conv_id, request_id) =
        file_payment(ctx, "e2e: pay for the parking", 1200, "pending").await?;
    let token = issue_link(ctx, &request_id).await?;
    // No card fields at all: declining must not demand one.
    let declined = submit(ctx, &token, &[("decision", "decline")]).await?;
    if declined.status() != 200 || !declined.text().await?.contains("Not paid") {
        bail!("declining did not succeed");
    }
    let (status, last4, _) = status_of(ctx, &request_id)?;
    if status != "declined" || last4.is_some() {
        bail!("decline recorded wrongly: {status} {last4:?}");
    }
    if open_page(ctx, &token, true).await?.status() != 404 {
        bail!("the link still works after it was declined");
    }
    Ok(())
}

/// The agent buys the mooring, forgets, and asks again in a fresh
/// conversation. Nothing should reach the user's phone the second time
/// except the news that it was stopped.
///
/// The load-bearing assertions are the database ones: the row is `held`, it
/// names what it repeats, it has no link and cannot be given one. The
/// assistant's reply is scripted here — a no-model agent cannot react to a
/// tool result — so it proves the turn finished and spoke, not that a model
/// would say the right thing. The alert itself is delivered through
/// `PendingLinks`, which lives in the daemon's memory and is drained by the
/// Telegram and Slack loops; the HTTP surface this harness drives does not
/// drain it, and a second process cannot read it. `PaymentRequestTool`'s
/// unit test is what holds that half down.
async fn payment_duplicate_is_held_and_user_alerted(ctx: &Ctx) -> Result<()> {
    let (_paid_conv, paid) = file_payment(ctx, "e2e: pay for the harbour", 3300, "pending").await?;
    let token = issue_link(ctx, &paid).await?;
    let approved = submit(
        ctx,
        &token,
        &[
            ("decision", "approve"),
            ("holder", HOLDER),
            ("number", TEST_CARD),
            ("expiry", "12/40"),
            ("security_code", "987"),
        ],
    )
    .await?;
    if approved.status() != 200 || !approved.text().await?.contains("Approved") {
        bail!("approving the first payment did not succeed");
    }

    // Pressing pay needs Chrome and a card that lives in the daemon's
    // memory, neither of which this harness has, so the press is recorded
    // rather than made. The row is the whole of what the duplicate check
    // reads, so a written row is a faithful stand-in for a pressed one.
    {
        let conn = rusqlite::Connection::open(&ctx.db_path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        let moved = conn.execute(
            "UPDATE payment_requests SET status = 'used', used_at = ?2 WHERE id = ?1",
            rusqlite::params![&paid, chrono::Utc::now().timestamp_millis()],
        )?;
        if moved != 1 {
            bail!("could not record the first payment as made");
        }
    }

    let (conv_id, held) = file_payment(ctx, "e2e: pay for the harbour again", 3300, "held").await?;

    let conn = rusqlite::Connection::open_with_flags(
        &ctx.db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let (duplicate_of, confirmed, has_link): (Option<String>, i64, bool) = conn.query_row(
        "SELECT duplicate_of, duplicate_confirmed, link_token_hash IS NOT NULL
           FROM payment_requests WHERE id = ?1",
        [&held],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    if duplicate_of.as_deref() != Some(paid.as_str()) {
        bail!("the held request does not name what it repeats: {duplicate_of:?}");
    }
    if confirmed != 0 {
        bail!("a held request was somehow marked as a confirmed second payment");
    }
    if has_link {
        bail!("a held request was given an approval link");
    }
    if issue_link(ctx, &held).await.is_ok() {
        bail!("a held request could be given a link after the fact");
    }
    if status_of(ctx, &paid)?.0 != "used" {
        bail!("the payment it repeats was disturbed");
    }

    // And the turn ended rather than stalling on an approval that is never
    // coming.
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let messages: Value = ctx
            .get(&format!("/api/conversations/{conv_id}/messages"))
            .await?
            .json()
            .await?;
        let spoke = messages.as_array().is_some_and(|all| {
            all.iter().any(|m| {
                m["role"] == "assistant"
                    && m["content"]
                        .as_str()
                        .is_some_and(|c| c.contains("stopped that payment") && c.contains("repeat"))
            })
        });
        if spoke {
            return Ok(());
        }
        if std::time::Instant::now() > deadline {
            bail!("the held payment was never reported back to the user: {messages}");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
