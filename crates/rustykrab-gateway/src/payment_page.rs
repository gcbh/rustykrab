//! The page a user opens to approve one purchase and supply the card for it.
//!
//! The agent never holds a card. When it reaches a checkout it files a
//! payment request with the terms — merchant, the exact site, the most it
//! may pay — and the user is sent a link to this page. Approving here *is*
//! the consent: the card goes to the in-memory vault bound to those terms,
//! and the agent may then enter it on that site and press pay, but only if
//! the page total is within the approved amount.
//!
//! Built the same way as the credential page (`credential_page.rs`), for the
//! same reasons: server-rendered HTML outside `/api/`, no JavaScript near
//! the values, guarded by tailnet identity plus a one-time hashed token.
//! Differences, each deliberate:
//!
//! - **Autofill is on.** The inputs carry `cc-*` autocomplete tokens so a
//!   phone can fill a saved card. A login page turns autofill off because
//!   it is enrolling a secret the agent will reuse; this page is a checkout.
//! - **The terms are the headline.** The amount and the site the card is
//!   bound to are what the user is agreeing to, so they are shown above
//!   the fields and repeated on the button.
//! - **Declining is a first-class answer**, not closing the tab: it tells
//!   the agent not to pay rather than leaving the turn waiting.
//! - **Responses are `no-store`**, so a card page never sits in a cache.
//! - **A confirmed duplicate says so, above the form.** A request the user
//!   asked for after being told it repeats an earlier payment reaches this
//!   page looking exactly like a first purchase. The one thing that
//!   distinguishes it is on the row (`duplicate_confirmed`), so the page
//!   says it out loud: the approval given here is a *second* charge. A
//!   `held` request, by contrast, never reaches this page at all — it is
//!   issued no link, and `find_by_link` only returns `pending` rows.

use axum::extract::{Form, Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use rustykrab_core::Error;
use rustykrab_store::{CardDetails, PaymentRequest, PaymentStatus, CARD_TTL};
use std::collections::HashMap;

use crate::credential_page::{esc, page, tailnet_login};
use crate::AppState;

pub fn routes() -> axum::Router<AppState> {
    axum::Router::new().route("/p/{token}", axum::routing::get(show).post(submit))
}

fn no_store(status: StatusCode, html: String) -> Response {
    (status, [(header::CACHE_CONTROL, "no-store")], Html(html)).into_response()
}

/// One refusal for every reason, as on the credential page, so the page is
/// not an oracle for which links exist.
fn refused() -> Response {
    no_store(
        StatusCode::NOT_FOUND,
        page(
            "Link expired",
            "<p>This payment link is no longer valid. It may already have been \
             answered, or it may have expired.</p><p>Ask the agent to send a new \
             one.</p>",
        ),
    )
}

async fn show(
    State(state): State<AppState>,
    Path(token): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !state.credential_page_policy.permits(&headers) {
        tracing::warn!("payment page refused: no permitted tailnet identity");
        return refused();
    }
    let Ok(Some(request)) = state
        .agent
        .store
        .payment_requests()
        .find_by_link(&token)
        .await
    else {
        return refused();
    };
    let earlier = earlier_payment(&state, &request).await;
    no_store(
        StatusCode::OK,
        form_page(&request, &token, None, earlier.as_deref()),
    )
}

/// The payment a `duplicate_confirmed` request is about to repeat.
///
/// `None` for an ordinary request, and also when the earlier row cannot be
/// read: a lookup failure must not stop the user approving a purchase they
/// asked for, so the page falls back to its usual shape rather than to an
/// error.
async fn earlier_payment(
    state: &AppState,
    request: &PaymentRequest,
) -> Option<Box<PaymentRequest>> {
    if !request.duplicate_confirmed {
        return None;
    }
    let id = request.duplicate_of.as_deref()?;
    match state.agent.store.payment_requests().get(id).await {
        Ok(earlier) => Some(Box::new(earlier)),
        Err(e) => {
            tracing::warn!(error = %e, %id, "the payment this one repeats could not be read");
            None
        }
    }
}

/// What the user asked for when they pressed a button.
#[derive(Debug)]
enum Decision {
    Decline,
    Approve(Result<CardDetails, &'static str>),
}

/// Read the submitted form. Pure, so the parsing is testable without a
/// server.
fn decide(values: &HashMap<String, String>) -> Decision {
    if values.get("decision").map(String::as_str) == Some("decline") {
        return Decision::Decline;
    }
    let field = |k: &str| values.get(k).map(String::as_str).unwrap_or("");
    Decision::Approve(
        CardDetails::new(
            field("number"),
            field("expiry"),
            field("security_code"),
            field("holder"),
            values.get("postal_code").map(String::as_str),
        )
        .map_err(|e| e.message()),
    )
}

async fn submit(
    State(state): State<AppState>,
    Path(token): Path<String>,
    headers: HeaderMap,
    Form(values): Form<HashMap<String, String>>,
) -> Response {
    if !state.credential_page_policy.permits(&headers) {
        tracing::warn!("payment page submit refused: no permitted tailnet identity");
        return refused();
    }
    let payments = state.agent.store.payment_requests();
    let Ok(Some(request)) = payments.find_by_link(&token).await else {
        return refused();
    };
    let by = tailnet_login(&headers).unwrap_or_else(|| "payment page".to_string());

    match decide(&values) {
        Decision::Decline => match payments.decline(&request.id, &by).await {
            Ok(()) => no_store(
                StatusCode::OK,
                page(
                    "Not paid",
                    &format!(
                        "<p>Not paid. The agent has been told not to pay {}.</p>",
                        esc(&request.merchant)
                    ),
                ),
            ),
            Err(_) => refused(),
        },
        // Re-render with the reason and nothing the user typed: a card
        // number echoed into `value=""` would sit in the page source.
        Decision::Approve(Err(reason)) => {
            let earlier = earlier_payment(&state, &request).await;
            no_store(
                StatusCode::OK,
                form_page(&request, &token, Some(reason), earlier.as_deref()),
            )
        }
        Decision::Approve(Ok(card)) => match payments.authorize(&request.id, card, &by).await {
            Ok(()) => no_store(StatusCode::OK, approved_page(&request)),
            Err(Error::AlreadyExists(_)) => refused(),
            Err(e) => {
                tracing::error!(error = %e, "payment page could not record the approval");
                let earlier = earlier_payment(&state, &request).await;
                no_store(
                    StatusCode::OK,
                    form_page(
                        &request,
                        &token,
                        Some("That could not be saved. Try once more."),
                        earlier.as_deref(),
                    ),
                )
            }
        },
    }
}

fn form_page(
    request: &PaymentRequest,
    token: &str,
    error: Option<&str>,
    earlier: Option<&PaymentRequest>,
) -> String {
    let amount = request.amount.to_string();
    let mut body = String::new();
    body.push_str("<h1>Approve a payment</h1>");
    // Above everything else, because it changes what the rest of the page
    // means. The user asked for this second payment, but they asked for it
    // in a chat message some minutes ago, and the page they are now looking
    // at is indistinguishable from the one they approved the first time.
    if let Some(earlier) = earlier {
        body.push_str(&format!(
            "<div class=\"warn\"><b>This is a second payment.</b> You already {} {} to {} \
             on {}. Approving here pays it again.</div>",
            match earlier.status {
                PaymentStatus::Used | PaymentStatus::Paying => "paid",
                _ => "approved",
            },
            esc(&earlier.amount.to_string()),
            esc(&earlier.merchant),
            esc(&rustykrab_store::stamp_utc(
                earlier.used_at.unwrap_or(earlier.created_at)
            )),
        ));
    }
    body.push_str(&format!(
        "<p class=\"amount\">{}</p><p class=\"merchant\">to <b>{}</b></p>",
        esc(&amount),
        esc(&request.merchant)
    ));
    if let Some(description) = request.description.as_deref() {
        body.push_str(&format!("<p class=\"why\">{}</p>", esc(description)));
    }
    body.push_str(&format!(
        "<p class=\"site\">Only on <code>{}</code></p>",
        esc(&request.origin)
    ));
    if let Some(err) = error {
        body.push_str(&format!("<p class=\"err\">{}</p>", esc(err)));
    }
    body.push_str(&format!(
        "<form method=\"post\" action=\"/p/{}\" autocomplete=\"on\">",
        esc(token)
    ));
    for (label, name, autocomplete, extra) in [
        (
            "Name on card",
            "holder",
            "cc-name",
            "autocapitalize=\"words\" required",
        ),
        (
            "Card number",
            "number",
            "cc-number",
            "inputmode=\"numeric\" autocorrect=\"off\" spellcheck=\"false\" required",
        ),
        (
            "Expiry",
            "expiry",
            "cc-exp",
            "inputmode=\"numeric\" placeholder=\"MM/YY\" required",
        ),
        (
            "Security code",
            "security_code",
            "cc-csc",
            "inputmode=\"numeric\" required",
        ),
        (
            "Billing ZIP / postal code (if the checkout asks)",
            "postal_code",
            "postal-code",
            "autocapitalize=\"characters\"",
        ),
    ] {
        body.push_str(&format!(
            "<label>{label}<input name=\"{name}\" type=\"text\" autocomplete=\"{autocomplete}\" {extra}></label>"
        ));
    }
    body.push_str(&format!(
        "<button type=\"submit\" name=\"decision\" value=\"approve\">Pay up to {}</button>\
         <button type=\"submit\" name=\"decision\" value=\"decline\" class=\"secondary\" \
         formnovalidate>Don't pay</button></form>",
        esc(&amount)
    ));
    body.push_str(&format!(
        "<p class=\"note\">The agent never sees these numbers. They are held in \
         memory on this Mac, can only be entered on {} or its payment provider's \
         card fields, and only for a total up to {}. They are erased as soon as \
         it pays, or after {} minutes. Nothing is saved.</p>",
        esc(&request.origin),
        esc(&amount),
        CARD_TTL.as_secs() / 60
    ));
    page("Approve payment", &body)
}

fn approved_page(request: &PaymentRequest) -> String {
    page(
        "Approved",
        &format!(
            "<p>Approved. The agent can now pay {} up to {} on <code>{}</code>.</p>\
             <p>The card is erased once it pays, or in {} minutes if it doesn't. \
             You can close this page.</p>",
            esc(&request.merchant),
            esc(&request.amount.to_string()),
            esc(&request.origin),
            CARD_TTL.as_secs() / 60
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustykrab_store::Money;

    fn request() -> PaymentRequest {
        PaymentRequest {
            id: "r1".into(),
            conversation_id: None,
            merchant: "Steamship <Authority>".into(),
            origin: "https://www.steamshipauthority.com".into(),
            amount: Money::parse("46.00", "USD").unwrap(),
            description: Some("2 passengers".into()),
            status: PaymentStatus::Pending,
            created_at: 0,
            card_last4: None,
            used_at: None,
            duplicate_of: None,
            duplicate_confirmed: false,
        }
    }

    fn form(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn the_terms_are_shown_and_escaped() {
        let html = form_page(&request(), "tok", None, None);
        assert!(html.contains("USD 46.00"));
        assert!(html.contains("Pay up to USD 46.00"));
        assert!(html.contains("<code>https://www.steamshipauthority.com</code>"));
        assert!(html.contains("Steamship &lt;Authority&gt;"));
        assert!(!html.contains("<Authority>"));
        assert!(html.contains("action=\"/p/tok\""));
    }

    /// Phone autofill needs the standard tokens; without them the user
    /// types sixteen digits on a phone keyboard.
    #[test]
    fn inputs_carry_card_autofill_tokens() {
        let html = form_page(&request(), "tok", None, None);
        for token in ["cc-name", "cc-number", "cc-exp", "cc-csc", "postal-code"] {
            assert!(
                html.contains(&format!("autocomplete=\"{token}\"")),
                "{token}"
            );
        }
        assert!(
            html.contains("formnovalidate"),
            "declining must not demand a card"
        );
    }

    #[test]
    fn declining_needs_no_card_and_approving_needs_a_valid_one() {
        assert!(matches!(
            decide(&form(&[("decision", "decline")])),
            Decision::Decline
        ));
        assert!(matches!(
            decide(&form(&[("decision", "approve"), ("number", "4242")])),
            Decision::Approve(Err("That card number doesn't look right."))
        ));
        match decide(&form(&[
            ("decision", "approve"),
            ("holder", "Ada Lovelace"),
            ("number", "4242 4242 4242 4242"),
            ("expiry", "12/40"),
            ("security_code", "123"),
            ("postal_code", ""),
        ])) {
            Decision::Approve(Ok(card)) => {
                assert_eq!(card.last4(), "4242");
                assert_eq!(card.postal_code(), None, "a blank optional field is absent");
            }
            other => panic!("expected an approval, got {other:?}"),
        }
    }

    /// A second payment the user asked for looks exactly like a first one.
    /// The row knows the difference, so the page has to say it.
    #[test]
    fn a_confirmed_duplicate_warns_that_this_pays_twice() {
        let mut second = request();
        second.duplicate_confirmed = true;
        second.duplicate_of = Some("r0".into());
        let mut earlier = request();
        earlier.id = "r0".into();
        earlier.status = PaymentStatus::Used;
        // 2026-09-11 16:03 UTC.
        earlier.used_at = Some(1_789_142_580_000);

        let html = form_page(&second, "tok", None, Some(&earlier));
        assert!(html.contains("This is a second payment"), "{html}");
        assert!(html.contains("You already paid USD 46.00"), "{html}");
        assert!(html.contains("2026-09-11 16:03 UTC"), "{html}");
        assert!(
            html.contains("Steamship &lt;Authority&gt;") && !html.contains("<Authority>"),
            "the earlier merchant is escaped too"
        );
        assert!(
            html.find("second payment").unwrap() < html.find("<form").unwrap(),
            "the warning has to come before the fields"
        );

        assert!(
            !form_page(&request(), "tok", None, None).contains("second payment"),
            "an ordinary request must not cry wolf"
        );
    }

    /// A rejected form is re-rendered from the request alone, so nothing
    /// the user typed can be in it.
    #[test]
    fn an_error_rerender_never_echoes_the_card() {
        let html = form_page(&request(), "tok", Some("That card has expired."), None);
        assert!(html.contains("That card has expired."));
        let inputs: Vec<&str> = html
            .split("<input")
            .skip(1)
            .map(|rest| rest.split('>').next().unwrap_or(rest))
            .collect();
        assert_eq!(inputs.len(), 5);
        assert!(
            inputs.iter().all(|input| !input.contains("value=")),
            "no input is pre-filled"
        );
    }
}
