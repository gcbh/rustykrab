use async_trait::async_trait;
use rustykrab_core::active_tools::with_session_context;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Error, Result, Tool};
use rustykrab_store::{Money, PaymentRequestStore, PaymentTerms, CARD_TTL};
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
        let id = self
            .payments
            .file(
                PaymentTerms {
                    merchant: merchant.to_string(),
                    origin: origin.clone(),
                    amount: amount.clone(),
                    description,
                },
                conversation_id,
            )
            .await
            .map_err(|e| {
                Error::ToolExecution(format!("could not file the payment request: {e}").into())
            })?;

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
