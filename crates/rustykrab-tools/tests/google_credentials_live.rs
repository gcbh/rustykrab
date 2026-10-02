//! Live proof that a credential Google *rejects* asks for a new one.
//!
//! Ignored by default — needs outbound network to `imap.gmail.com:993` and
//! `www.google.com` (see `caldav`'s module docs for why the CalDAV host is
//! the legacy one):
//!
//! ```sh
//! cargo test -p rustykrab-tools --test google_credentials_live \
//!   -- --ignored --nocapture --test-threads=1
//! ```
//!
//! No real account or password is required, and that is the point. The
//! condition under test is a *well-formed credential the server refuses*,
//! and a deliberately wrong sixteen-character app password produces exactly
//! the response a revoked one does: `[AUTHENTICATIONFAILED]` from IMAP, 401
//! from CalDAV. So this reproduces the daily-briefing failure end to end
//! without a secret ever entering the repo.
//!
//! Unit tests cover the pieces. What only a live run can show is that
//! `load` really does hand this credential through — that the ask fires from
//! the network path and not from a local check that happened to catch it —
//! which is the whole distinction the fix turns on.
//!
//! The link is delivered out of band, so these assert the *opposite* of the
//! obvious thing: the error handed back to the model must contain no URL at
//! all, and the URL must instead be sitting in `PendingLinks` for the
//! conversation. Handing a 64-hex token to a local model was measured losing
//! nine characters of it, and a truncated token renders the same "Link
//! expired" page a real expiry does — so "no URL in the message" is the
//! property worth pinning.

use std::sync::Arc;

use rustykrab_core::active_tools::{SessionToolContext, SESSION_TOOL_CONTEXT};
use rustykrab_core::capability::CapabilitySet;
use rustykrab_core::{Error, Tool, ToolErrorKind};
use rustykrab_store::{credential_backend::MemoryBackend, PendingLinks, Store};
use rustykrab_tools::{CalDavTool, GmailTool};
use serde_json::json;
use uuid::Uuid;

/// Well formed, sixteen alphanumerics, and wrong. Everything
/// `google_credentials::validate` can check locally passes.
const BOGUS_PASSWORD: &str = "abcdefghijklmnop";
const ADDRESS: &str = "rustykrab-live-test@gmail.com";

/// Where a minted link points. Set here rather than relied upon from the
/// environment so the assertion that a link *was* minted cannot pass or fail
/// for reasons outside the test.
const PUBLIC_URL: &str = "https://rustykrab.test";

/// Install the TLS provider the daemon installs at startup.
///
/// `rustykrab-cli` does this in `main` before any rustls use. A test binary
/// has no such entry point, and with both `ring` and `aws-lc-rs` reachable
/// in the dependency graph rustls refuses to pick one. Choosing `ring` — the
/// same one production installs — means the IMAP handshake here is the
/// handshake the daemon performs.
fn install_tls_provider() {
    // Err only means another test in this binary got here first.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// A store on a tempdir the caller keeps alive, seeded with the bad
/// credential.
///
/// The credential backend is in-memory on purpose: the real one on macOS is
/// the login keychain, which would prompt for access mid-test and leave a
/// password on the machine afterwards.
async fn seeded_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path(), vec![7u8; 32])
        .expect("open store")
        .with_credential_backend(Arc::new(MemoryBackend::new()));

    let secrets = store.secrets();
    secrets.create("gmail_email", ADDRESS).await.unwrap();
    secrets
        .create("gmail_app_password", BOGUS_PASSWORD)
        .await
        .unwrap();

    (dir, store)
}

/// Run `f` as if inside the agent runner, so tools can see which
/// conversation they belong to.
///
/// `ask` reads the conversation id from this context and a link cannot be
/// queued without one — outside a runner scope the out-of-band path silently
/// degrades to "a prompt is waiting in the Apollo app". A test that skipped
/// this would still pass while proving nothing about delivery.
async fn in_conversation<F, T>(conversation_id: Uuid, f: F) -> T
where
    F: std::future::Future<Output = T>,
{
    let ctx = SessionToolContext {
        conversation_id,
        capabilities: Arc::new(CapabilitySet::none()),
        all_tools: Arc::new(Vec::new()),
        active_tools: Arc::new(Default::default()),
        recall: Arc::new(Default::default()),
        todos: Arc::new(Default::default()),
    };
    SESSION_TOOL_CONTEXT.scope(ctx, f).await
}

/// The link reached the queue, and never reached the model.
///
/// Checked against the queued token rather than against "does the message
/// contain a URL": both services put legitimate URLs in their refusals — the
/// CalDAV request URL, and Google's own support.google.com/BadCredentials
/// link in the SMTP 535 — so a blanket URL ban fails on correct behaviour.
/// The property that matters is narrower: this specific capture token must
/// not be anywhere the model can see it.
fn assert_link_queued_not_spoken(links: &PendingLinks, conv: Uuid, message: &str) {
    let queued = links.take(conv);
    assert_eq!(queued.len(), 1, "expected one queued link, got {queued:?}");
    let link = &queued[0];
    assert!(
        link.starts_with(&format!("{PUBLIC_URL}/c/")),
        "queued link is not a credential link: {link}"
    );

    assert!(
        !message.contains(link),
        "the model was handed the capture URL: {message}"
    );
    // The token alone, in case the URL was assembled differently. This is
    // the part that must never enter the context window or a transcript.
    let token = link.rsplit('/').next().expect("link has a token");
    assert!(
        !message.contains(token),
        "the capture token leaked into the model's context: {message}"
    );
}

/// Assert the shape every rejected-credential error has to have, and return
/// the message so the caller can look for the link.
fn assert_asks_rather_than_reports(err: &Error) -> String {
    let Error::ToolExecution(tool_error) = err else {
        panic!("expected a tool error, got {err:?}");
    };
    assert_eq!(
        tool_error.kind,
        ToolErrorKind::PermissionDenied,
        "a rejected credential must not be retryable — the runner repeats an \
         untyped error three times, and each repeat mints another one-time \
         link: {tool_error:?}"
    );
    tool_error.message.clone()
}

#[tokio::test]
#[ignore = "requires outbound network to Gmail"]
async fn gmail_rejecting_the_stored_password_files_a_request_with_a_link() {
    install_tls_provider();
    std::env::set_var("RUSTYKRAB_PUBLIC_URL", PUBLIC_URL);
    let (_dir, store) = seeded_store().await;
    let requests = store.credential_requests();

    let links = PendingLinks::new();
    let conv = Uuid::new_v4();
    let tool = GmailTool::new(store.guarded_secrets())
        .with_requests(requests.clone())
        .with_pending_links(links.clone());
    let err = in_conversation(conv, async {
        tool.execute(json!({"action": "search", "query": "ALL", "max_results": 1}))
            .await
            .expect_err("Gmail must refuse a wrong app password")
    })
    .await;

    let message = assert_asks_rather_than_reports(&err);
    eprintln!("gmail error: {message}");

    // The precondition the fix exists for: this credential got as far as
    // Google, so the refusal is the server's and not a local guess.
    assert!(
        message.contains("IMAP login failed"),
        "the ask must be built from Google's own refusal: {message}"
    );

    let pending = requests.pending().await.unwrap();
    assert_eq!(pending.len(), 1, "expected one request, got {pending:?}");
    assert_eq!(pending[0].name, "gmail_app_password");
    assert_eq!(pending[0].service.as_deref(), Some("Gmail"));

    // What the user actually receives. Before the fix this sentence was
    // "please update gmail_app_password in the credential store", which is
    // not something anyone can act on from a Telegram thread. It is now a
    // link — delivered beside the message rather than inside it.
    assert_link_queued_not_spoken(&links, conv, &message);
}

#[tokio::test]
#[ignore = "requires outbound network to Google CalDAV"]
async fn caldav_rejecting_the_stored_password_files_a_request_with_a_link() {
    install_tls_provider();
    std::env::set_var("RUSTYKRAB_PUBLIC_URL", PUBLIC_URL);
    let (_dir, store) = seeded_store().await;
    let requests = store.credential_requests();

    let links = PendingLinks::new();
    let conv = Uuid::new_v4();
    let tool = CalDavTool::new(store.guarded_secrets())
        .with_requests(requests.clone())
        .with_pending_links(links.clone());
    let err = in_conversation(conv, async {
        tool.execute(json!({"action": "list_events"}))
            .await
            .expect_err("Google must refuse a wrong app password")
    })
    .await;

    let message = assert_asks_rather_than_reports(&err);
    eprintln!("caldav error: {message}");

    assert!(
        message.contains("401"),
        "the ask must be built from Google's own refusal: {message}"
    );
    assert_link_queued_not_spoken(&links, conv, &message);

    let pending = requests.pending().await.unwrap();
    assert_eq!(pending.len(), 1, "expected one request, got {pending:?}");
    assert_eq!(pending[0].name, "gmail_app_password");
}

#[tokio::test]
#[ignore = "requires outbound network to Gmail and Google CalDAV"]
async fn one_dead_password_is_one_ask_however_many_tools_trip_over_it() {
    install_tls_provider();
    std::env::set_var("RUSTYKRAB_PUBLIC_URL", PUBLIC_URL);
    let (_dir, store) = seeded_store().await;
    let requests = store.credential_requests();

    let links = PendingLinks::new();
    let conv = Uuid::new_v4();
    let gmail = GmailTool::new(store.guarded_secrets())
        .with_requests(requests.clone())
        .with_pending_links(links.clone());
    let caldav = CalDavTool::new(store.guarded_secrets())
        .with_requests(requests.clone())
        .with_pending_links(links.clone());

    // The briefing calls both, and previously each would have retried three
    // times. Four refusals must still leave the user with one thing to do.
    in_conversation(conv, async {
        gmail
            .execute(json!({"action": "search", "query": "ALL", "max_results": 1}))
            .await
            .expect_err("gmail");
        gmail
            .execute(json!({"action": "labels"}))
            .await
            .expect_err("gmail again");
        caldav
            .execute(json!({"action": "list_events"}))
            .await
            .expect_err("caldav");
        caldav
            .execute(json!({"action": "list_calendars"}))
            .await
            .expect_err("caldav again");
    })
    .await;

    let pending = requests.pending().await.unwrap();
    assert_eq!(
        pending.len(),
        1,
        "one credential, one prompt — got {pending:?}"
    );

    // And one link. The request deduped before this was fixed, but every
    // call still minted its own, and superseding kills the token on the row
    // it replaces — so the user received four messages of which three were
    // already dead, indistinguishable from the live one until tapped. This
    // was 4 before `has_live_link` guarded the ask.
    let queued = links.take(conv);
    assert_eq!(
        queued.len(),
        1,
        "one dead password must yield one link, not one per tool call: {queued:?}"
    );
}

#[tokio::test]
#[ignore = "requires outbound network to Gmail SMTP"]
async fn smtp_rejecting_the_stored_password_files_a_request_with_a_link() {
    install_tls_provider();
    std::env::set_var("RUSTYKRAB_PUBLIC_URL", PUBLIC_URL);
    let (_dir, store) = seeded_store().await;
    let requests = store.credential_requests();

    let links = PendingLinks::new();
    let conv = Uuid::new_v4();
    let tool = GmailTool::new(store.guarded_secrets())
        .with_requests(requests.clone())
        .with_pending_links(links.clone());
    let err = in_conversation(conv, async {
        tool.execute(json!({
            "action": "send",
            "to": "nobody@example.com",
            "subject": "live credential test",
            "body": "This send must never leave the building.",
        }))
        .await
        .expect_err("Gmail SMTP must refuse a wrong app password")
    })
    .await;

    let message = assert_asks_rather_than_reports(&err);
    eprintln!("smtp error: {message}");

    // Sending is the path that classifies by reply code rather than by a
    // distinct error, so this is the one that proves `535` really does land
    // in the authentication family and not somewhere near the 550s.
    assert!(
        message.contains("SMTP send failed"),
        "the ask must be built from Google's own refusal: {message}"
    );
    assert_link_queued_not_spoken(&links, conv, &message);

    let pending = requests.pending().await.unwrap();
    assert_eq!(pending.len(), 1, "expected one request, got {pending:?}");
    assert_eq!(pending[0].name, "gmail_app_password");
}
