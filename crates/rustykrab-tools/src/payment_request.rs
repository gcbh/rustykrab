use async_trait::async_trait;
use rustykrab_core::active_tools::with_session_context;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Error, Result, Tool};
use rustykrab_store::{
    Money, PaymentRequest, PaymentRequestStore, PaymentStatus, PaymentTerms, CARD_TTL,
};
use serde_json::{json, Value};

/// Asks the user to approve one purchase and supply the card for it.
///
/// The counterpart to `credential_request` for money. Before this existed
/// the agent had nowhere to go at a checkout: the credential tool told it
/// payment cards were unsupported, so it stopped and asked the user to pay
/// themselves — correct for a login flow, and a dead end for "book the
/// 2:25pm ferry for two".
///
/// The request carries the terms, not a field list. The user sees the
/// merchant, the exact site and the amount on a one-time page, and approving
/// there is the consent: the card may be entered on that site only, for a
/// total no higher than that amount, once. The browser enforces all three;
/// this tool only asks.
pub struct PaymentRequestTool {
    payments: PaymentRequestStore,
    pending_links: Option<rustykrab_store::PendingLinks>,
    /// Overrides `RUSTYKRAB_PUBLIC_URL`, so tests need not mutate the
    /// process environment.
    public_base: Option<String>,
}

impl PaymentRequestTool {
    pub fn new(payments: PaymentRequestStore) -> Self {
        Self {
            payments,
            pending_links: None,
            public_base: None,
        }
    }

    /// Deliver the approval link out of band, after the turn.
    pub fn with_pending_links(mut self, links: rustykrab_store::PendingLinks) -> Self {
        self.pending_links = Some(links);
        self
    }

    #[cfg(test)]
    fn with_public_base(mut self, base: &str) -> Self {
        self.public_base = Some(base.to_string());
        self
    }
}

/// What the user is told when a payment is held as a probable duplicate.
///
/// Queued through `PendingLinks`, the same out-of-band path an approval
/// link takes, and for a stronger version of the same reason. A hold is the
/// one moment in this flow where the model is *wrong about what it is
/// doing* — it believes it is buying something — so making it the messenger
/// is asking the party that just made the mistake to report it. This string
/// reaches the user whatever the model then says.
///
/// It names the amount, the merchant, the site and the time, because "this
/// looks like a duplicate" is unanswerable on its own: only the user knows
/// whether the ferry they booked this morning is the ferry the agent is
/// trying to book now. It never carries a card, a link or a request id.
pub(crate) fn duplicate_alert(prior: &PaymentRequest) -> String {
    let when = rustykrab_store::stamp_utc(prior.used_at.unwrap_or(prior.created_at));
    let state = match prior.status {
        PaymentStatus::Used => "already paid",
        PaymentStatus::Paying => "being paid right now",
        PaymentStatus::Authorized => "already approved and waiting to be paid",
        PaymentStatus::Pending => "already waiting for your approval",
        // A third attempt: the second one is still sitting held.
        _ => "already held as a duplicate",
    };
    format!(
        "⚠️ Payment stopped. This looks like a repeat of {} to {} at {}, which was {} \
         ({when}). Nothing has been paid and no approval link was sent. If you do want to \
         pay a second time, reply and say so.",
        prior.amount, prior.merchant, prior.origin, state,
    )
}

/// Wording for the model's next turn. Never contains the link: it is
/// delivered separately, for the reasons in `pending_links.rs`.
fn next_step(queued: bool, merchant: &str, amount: &Money, origin: &str) -> String {
    if queued {
        format!(
            "Tell the user, in one or two sentences, that you have sent them a secure link to \
             approve paying {merchant} up to {amount}, and that it works once and expires in \
             15 minutes. Do NOT write a URL yourself — it is being sent separately. Never ask \
             for card details in chat. Stop this task now; you will be resumed when they approve."
        )
    } else {
        format!(
            "No approval link could be sent (RUSTYKRAB_PUBLIC_URL is not configured). Tell the \
             user you have got the purchase as far as payment on {origin} and that they need to \
             finish paying {merchant} themselves. Never ask for card details in chat. Stop."
        )
    }
}

#[async_trait]
impl Tool for PaymentRequestTool {
    fn name(&self) -> &str {
        "payment_request"
    }

    fn description(&self) -> &str {
        "Ask the user to approve a purchase and supply the card for it. Use this \
         when something you are doing for the user — booking a ticket, placing an \
         order — reaches a checkout that needs payment. Do not tell the user you \
         cannot pay, and never ask for card details in chat.\n\n\
         First get to the payment step so you know the exact total. Then call this \
         with the checkout page's URL, the merchant, the total and its currency. The \
         user gets a secure link showing the merchant, the site and the amount; \
         approving there allows one payment of at most that amount on that site, and \
         you are resumed. You then enter the card with browser(action='fill_payment', \
         ref=..., field=...) and submit with browser(action='pay', ref=...). You never \
         see the card.\n\n\
         'url' is the checkout page (the card can only be entered on that exact site). \
         'amount' is the total shown at checkout, e.g. \"46.00\", and 'currency' its \
         ISO code, e.g. \"USD\". 'description' says what is being bought, in words the \
         user will recognise. A new request replaces any earlier one in this \
         conversation, so re-file if the total changes.\n\n\
         If this site has already been paid this amount today, the request is \
         held instead: nothing is sent to the user for approval, they are told \
         it was stopped, and you must stop the task rather than trying again. \
         Only if the user then explicitly asks to pay a second time, call this \
         again with confirm_duplicate: true.\n\n\
         Example:\n\
         {\"url\": \"https://www.steamshipauthority.com/reservations/checkout\", \
         \"merchant\": \"Steamship Authority\", \"amount\": \"46.00\", \"currency\": \
         \"USD\", \"description\": \"2 passenger tickets, Hyannis to Nantucket, Thu Sep \
         17, 2:25pm\"}"
    }

    /// Nothing more can happen until the user answers.
    fn blocks_turn(&self) -> bool {
        true
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The checkout page's URL. The card may only be entered on this site."
                    },
                    "merchant": {
                        "type": "string",
                        "description": "Who is being paid, as the user knows them, e.g. 'Steamship Authority'."
                    },
                    "amount": {
                        "type": "string",
                        "description": "The total shown at checkout, e.g. '46.00'. The browser refuses to pay a higher total."
                    },
                    "currency": {
                        "type": "string",
                        "description": "ISO 4217 code of the total, e.g. 'USD'."
                    },
                    "description": {
                        "type": "string",
                        "description": "What is being bought, e.g. '2 passenger tickets, Hyannis to Nantucket, Sep 17 2:25pm'."
                    },
                    "confirm_duplicate": {
                        "type": "boolean",
                        "description": "Only after the user has been told a payment looks like a duplicate and has explicitly asked to pay again. Setting it any other time has no effect."
                    }
                },
                "required": ["url", "merchant", "amount", "currency"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let url = args["url"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing 'url' parameter".into()))?;
        let merchant = args["merchant"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing 'merchant' parameter".into()))?;
        let currency = args["currency"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing 'currency' parameter".into()))?;
        // Models send `46` as often as `"46.00"`.
        let amount = match &args["amount"] {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => {
                return Err(Error::ToolExecution(
                    "missing 'amount' parameter (the checkout total, e.g. \"46.00\")".into(),
                ))
            }
        };
        let amount = Money::parse(&amount, currency)
            .map_err(|e| Error::ToolExecution(e.to_string().into()))?;

        let origin = crate::origin_key::canonical_credential_origin(url)?;
        // Loopback HTTP is how the harness serves a checkout; the browser
        // still refuses it unless its private-network test policy is on.
        let parsed = url::Url::parse(&origin).expect("canonical origin parses");
        let loopback = matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        if parsed.scheme() != "https" && !loopback {
            return Err(Error::ToolExecution(
                "a card can only be approved for an HTTPS checkout; do not continue on this site"
                    .into(),
            ));
        }

        let conversation_id = with_session_context(|c| c.conversation_id);
        let description = args["description"].as_str().map(str::to_string);
        let filed = self
            .payments
            .file(
                PaymentTerms {
                    merchant: merchant.to_string(),
                    origin: origin.clone(),
                    amount: amount.clone(),
                    description,
                    confirm_duplicate: args["confirm_duplicate"].as_bool().unwrap_or(false),
                },
                conversation_id,
            )
            .await
            .map_err(|e| {
                Error::ToolExecution(format!("could not file the payment request: {e}").into())
            })?;
        let id = filed.id;

        // A held request has no link and must not get one: the user is not
        // being asked to approve anything, they are being told the agent
        // was stopped. Minting one anyway would put a live approval page
        // for a purchase already made on the user's phone, which is the
        // outcome the hold exists to prevent.
        if let Some(prior) = filed.held_as_duplicate_of {
            let alerted = match (&self.pending_links, conversation_id) {
                (Some(pending), Some(conv)) => {
                    pending.push(conv, duplicate_alert(&prior));
                    true
                }
                _ => false,
            };
            return Ok(json!({
                "status": "held",
                "request_id": id,
                "duplicate_of": {
                    "merchant": prior.merchant,
                    "amount": prior.amount.to_string(),
                    "site": prior.origin,
                    "status": prior.status.as_str(),
                    "when": rustykrab_store::stamp_utc(
                        prior.used_at.unwrap_or(prior.created_at)
                    ),
                },
                "user_alerted": alerted,
                "next_step": format!(
                    "Nothing was paid and no approval link was sent. Tell the user, in one \
                     or two sentences, that you stopped the payment because it looks like a \
                     repeat of {} to {} that was already made. Do not file this request \
                     again and do not try to pay another way. Stop this task now. If the \
                     user replies that they do want to pay a second time, call \
                     payment_request again with confirm_duplicate: true.",
                    prior.amount, prior.merchant,
                ),
            }));
        }

        let base = self
            .public_base
            .clone()
            .or_else(crate::credential_link::public_base);
        let link = crate::credential_link::mint_payment_link(&self.payments, &id, base).await;
        let queued = match (&self.pending_links, conversation_id, link) {
            (Some(pending), Some(conv), Some(url)) => {
                pending.push(conv, url);
                true
            }
            _ => false,
        };

        Ok(json!({
            "status": "requested",
            "request_id": id,
            "merchant": merchant,
            "site": origin,
            "amount": amount.to_string(),
            "approval_valid_for_minutes": CARD_TTL.as_secs() / 60,
            "link_sent_separately": queued,
            "next_step": next_step(queued, merchant, &amount, &origin),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustykrab_core::active_tools::{SessionToolContext, SESSION_TOOL_CONTEXT};
    use rustykrab_store::{credential_backend::MemoryBackend, PaymentStatus, PendingLinks, Store};
    use std::sync::Arc;
    use uuid::Uuid;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), vec![7u8; 32])
            .unwrap()
            .with_credential_backend(Arc::new(MemoryBackend::new()));
        (dir, store)
    }

    async fn in_conversation<T>(conv: Uuid, f: impl std::future::Future<Output = T>) -> T {
        let ctx = SessionToolContext {
            conversation_id: conv,
            capabilities: Arc::new(rustykrab_core::CapabilitySet::none()),
            all_tools: Arc::new(Vec::new()),
            active_tools: Arc::new(Default::default()),
            recall: Arc::new(Default::default()),
            todos: Arc::new(Default::default()),
        };
        SESSION_TOOL_CONTEXT.scope(ctx, f).await
    }

    fn ferry() -> Value {
        json!({
            "url": "https://www.steamshipauthority.com/reservations/checkout?step=pay",
            "merchant": "Steamship Authority",
            "amount": 46,
            "currency": "usd",
            "description": "2 passenger tickets"
        })
    }

    #[tokio::test]
    async fn filing_records_the_terms_and_sends_the_link_out_of_band() {
        let (_dir, store) = store();
        let links = PendingLinks::new();
        let tool = PaymentRequestTool::new(store.payment_requests())
            .with_pending_links(links.clone())
            .with_public_base("https://mac.example.ts.net");
        let conv = Uuid::new_v4();

        let out = in_conversation(conv, tool.execute(ferry())).await.unwrap();

        let request = store
            .payment_requests()
            .get(out["request_id"].as_str().unwrap())
            .await
            .unwrap();
        assert_eq!(request.status, PaymentStatus::Pending);
        assert_eq!(request.origin, "https://www.steamshipauthority.com");
        assert_eq!(request.amount.to_string(), "USD 46.00");
        assert_eq!(request.conversation_id, Some(conv.to_string()));
        assert_eq!(out["link_sent_separately"], true);

        let queued = links.take(conv);
        assert_eq!(queued.len(), 1);
        assert!(queued[0].starts_with("https://mac.example.ts.net/p/"));
        let token = queued[0].rsplit('/').next().unwrap();
        assert!(
            !out.to_string().contains(token),
            "the model must never see the link"
        );
        assert!(store
            .payment_requests()
            .find_by_link(token)
            .await
            .unwrap()
            .is_some());
        assert!(out["next_step"].as_str().unwrap().contains("Stop"));
    }

    #[tokio::test]
    async fn without_a_public_url_the_agent_is_told_to_hand_payment_back() {
        let (_dir, store) = store();
        let tool = PaymentRequestTool::new(store.payment_requests())
            .with_pending_links(PendingLinks::new());
        // Only meaningful when the environment has no base either.
        if crate::credential_link::public_base().is_some() {
            return;
        }
        let out = in_conversation(Uuid::new_v4(), tool.execute(ferry()))
            .await
            .unwrap();
        assert_eq!(out["link_sent_separately"], false);
        assert!(out["next_step"].as_str().unwrap().contains("finish paying"));
    }

    /// The hold's whole point is that the user hears about it. The model is
    /// being told to stop, so the message cannot depend on the model.
    #[tokio::test]
    async fn a_held_duplicate_alerts_the_user_and_mints_no_link() {
        let (_dir, store) = store();
        let links = PendingLinks::new();
        let tool = PaymentRequestTool::new(store.payment_requests())
            .with_pending_links(links.clone())
            .with_public_base("https://mac.example.ts.net");
        let payments = store.payment_requests();

        // Bought once, in an earlier conversation.
        let bought = Uuid::new_v4();
        let first = in_conversation(bought, tool.execute(ferry()))
            .await
            .unwrap()["request_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(links.take(bought).len(), 1, "the first ask sends a link");
        payments
            .authorize(
                &first,
                rustykrab_store::CardDetails::new("4242424242424242", "12/40", "987", "Ada", None)
                    .unwrap(),
                "me",
            )
            .await
            .unwrap();
        let claim = payments
            .claim_for_pay(&first)
            .await
            .unwrap()
            .expect("claim");
        drop(claim);
        payments.mark_used(&first).await.unwrap();

        // And asked for again, elsewhere.
        let asked_again = Uuid::new_v4();
        let out = in_conversation(asked_again, tool.execute(ferry()))
            .await
            .unwrap();

        assert_eq!(out["status"], "held");
        assert_eq!(out["duplicate_of"]["merchant"], "Steamship Authority");
        assert_eq!(out["duplicate_of"]["amount"], "USD 46.00");
        assert_eq!(out["duplicate_of"]["status"], "used");
        assert_eq!(out["user_alerted"], true);
        assert!(out["next_step"]
            .as_str()
            .unwrap()
            .contains("Stop this task"));
        assert!(
            out.get("link_sent_separately").is_none(),
            "a held request is not an ask, so nothing was sent to approve"
        );
        assert_eq!(
            payments
                .get(out["request_id"].as_str().unwrap())
                .await
                .unwrap()
                .status,
            PaymentStatus::Held
        );

        let queued = links.take(asked_again);
        assert_eq!(queued.len(), 1, "exactly one alert, and it is not a link");
        let alert = &queued[0];
        assert!(!alert.contains("/p/"), "{alert}");
        assert!(alert.contains("USD 46.00"), "{alert}");
        assert!(alert.contains("Steamship Authority"), "{alert}");
        assert!(alert.contains("already paid"), "{alert}");
        for leaked in ["4242", "987", &first] {
            assert!(
                !alert.contains(leaked),
                "'{leaked}' reached the user: {alert}"
            );
        }
    }

    /// The override the user gives after a hold. The store decides whether
    /// it means anything; the tool has to carry it.
    #[tokio::test]
    async fn the_confirm_duplicate_argument_reaches_the_store() {
        let (_dir, store) = store();
        let tool = PaymentRequestTool::new(store.payment_requests())
            .with_pending_links(PendingLinks::new());
        let conv = Uuid::new_v4();

        let mut args = ferry();
        args["confirm_duplicate"] = json!(true);
        // Nothing has been held here, so the flag is inert and this is an
        // ordinary first ask — which is exactly the protection.
        let out = in_conversation(conv, tool.execute(args)).await.unwrap();
        assert_eq!(out["status"], "requested");
        let filed = store
            .payment_requests()
            .get(out["request_id"].as_str().unwrap())
            .await
            .unwrap();
        assert_eq!(filed.status, PaymentStatus::Pending);
        assert!(
            !filed.duplicate_confirmed,
            "a model may not confirm a duplicate nobody stopped it for"
        );
    }

    #[tokio::test]
    async fn an_insecure_site_or_a_malformed_amount_is_refused() {
        let (_dir, store) = store();
        let tool = PaymentRequestTool::new(store.payment_requests());
        for insecure in [
            "http://shop.example.com/checkout",
            "http://localhost.shop.example.com/checkout",
        ] {
            let mut plain_http = ferry();
            plain_http["url"] = json!(insecure);
            assert!(tool.execute(plain_http).await.is_err(), "{insecure}");
        }

        for amount in [json!("forty six"), json!(-5), json!(null)] {
            let mut bad = ferry();
            bad["amount"] = amount.clone();
            assert!(tool.execute(bad).await.is_err(), "{amount}");
        }
        let mut bad_currency = ferry();
        bad_currency["currency"] = json!("dollars");
        assert!(tool.execute(bad_currency).await.is_err());
    }
}
