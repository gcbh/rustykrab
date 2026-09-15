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

use axum::extract::{Form, Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use rustykrab_core::Error;
use rustykrab_store::{CardDetails, PaymentRequest, CARD_TTL};
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
    no_store(StatusCode::OK, form_page(&request, &token, None))
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
            no_store(StatusCode::OK, form_page(&request, &token, Some(reason)))
        }
        Decision::Approve(Ok(card)) => match payments.authorize(&request.id, card, &by).await {
            Ok(()) => no_store(StatusCode::OK, approved_page(&request)),
            Err(Error::AlreadyExists(_)) => refused(),
            Err(e) => {
                tracing::error!(error = %e, "payment page could not record the approval");
                no_store(
                    StatusCode::OK,
                    form_page(
                        &request,
                        &token,
                        Some("That could not be saved. Try once more."),
                    ),
                )
            }
        },
    }
}

fn form_page(request: &PaymentRequest, token: &str, error: Option<&str>) -> String {
    let amount = request.amount.to_string();
    let mut body = String::new();
    body.push_str("<h1>Approve a payment</h1>");
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
    use rustykrab_store::{Money, PaymentStatus};

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
        let html = form_page(&request(), "tok", None);
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
        let html = form_page(&request(), "tok", None);
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

    /// A rejected form is re-rendered from the request alone, so nothing
    /// the user typed can be in it.
    #[test]
    fn an_error_rerender_never_echoes_the_card() {
        let html = form_page(&request(), "tok", Some("That card has expired."));
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
