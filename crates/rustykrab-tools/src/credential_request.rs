use async_trait::async_trait;
use rustykrab_core::active_tools::with_session_context;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Error, Result, Tool};
use rustykrab_store::{CredentialRequestStore, RequestedField};
use serde_json::{json, Value};

/// Asks the user for a credential the agent does not have.
///
/// This is the counterpart to `credential_write`: that tool stores a value
/// the agent already holds, while this one admits it holds nothing and
/// files a request the user answers from the Apollo app or WebChat, in a
/// masked field, over TLS. The value never passes through the model — which
/// is the point. A password typed into chat is a password in the
/// conversation history, in the context window, and in any transcript.
///
/// It is deliberately service-agnostic: `fields` describes whatever the
/// login needs, so a Gmail address and app password, a bare API token, and
/// a website's username and password are all the same request shape.
pub struct CredentialRequestTool {
    requests: CredentialRequestStore,
    /// Where a minted link waits until the turn has finished speaking.
    ///
    /// Optional so the tool still constructs without one; without it the
    /// request is filed and answerable in the app, there is simply no
    /// link to send.
    pending_links: Option<rustykrab_store::PendingLinks>,
}

impl CredentialRequestTool {
    pub fn new(requests: CredentialRequestStore) -> Self {
        Self {
            requests,
            pending_links: None,
        }
    }

    /// Deliver minted links out of band, after the turn.
    pub fn with_pending_links(mut self, links: rustykrab_store::PendingLinks) -> Self {
        self.pending_links = Some(links);
        self
    }
}

#[async_trait]
impl Tool for CredentialRequestTool {
    fn name(&self) -> &str {
        "credential_request"
    }

    fn description(&self) -> &str {
        "Ask the user to supply a credential you do not have. Use this the \
         moment you discover a credential is missing — a tool reported it is \
         not configured, credential_read found nothing, or a website you were \
         asked to use needs a login. The user gets a prompt in their app with \
         a secure field for each value.\n\n\
         Do NOT ask for passwords in chat, and never invent a value or tell \
         the user to run credential_write themselves — file this instead, then \
         tell them in one sentence that you have asked for it.\n\n\
         'name' is the credential this is about (it also dedupes repeat asks). \
         'service' is what the user recognises it as. 'fields' is one entry \
         per value you need, where 'key' is the credential name each answer is \
         stored under.\n\n\
         Example — Gmail needs two values:\n\
         {\"name\": \"gmail_app_password\", \"service\": \"Gmail\", \
         \"reason\": \"to search your inbox\", \"fields\": [\
         {\"key\": \"gmail_email\", \"label\": \"Gmail address\", \"secret\": false}, \
         {\"key\": \"gmail_app_password\", \"label\": \"App password\", \"secret\": true}]}\n\n\
         For a website login, name the keys after the site's host so the same \
         site always gets the same names: web_<host>_username and \
         web_<host>_password, with every dot and dash written as an \
         underscore. https://portal.example.com/login therefore gives \
         web_portal_example_com_username and web_portal_example_com_password. \
         Drop any leading 'www.'. Getting this right matters — the browser \
         looks the credential up under exactly this name.\n\n\
         Example — a website login:\n\
         {\"name\": \"web_portal_example_com_password\", \"service\": \
         \"portal.example.com\", \"reason\": \"to download your invoice\", \
         \"fields\": [\
         {\"key\": \"web_portal_example_com_username\", \"label\": \"Username\", \"secret\": false}, \
         {\"key\": \"web_portal_example_com_password\", \"label\": \"Password\", \"secret\": true}]}\n\n\
         Once stored, sign in with browser(action='fill_credential', ref=..., \
         field='username'|'password') — it types the value straight into the \
         page. Never read a password back with credential_read to type it \
         yourself: that puts it in this conversation, which is the one thing \
         this whole flow exists to avoid.\n\n\
         Payment cards do not go through this tool. When a checkout needs a \
         card, use payment_request instead, which asks the user to approve that \
         one purchase. Never request card numbers or security codes here or in \
         chat."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "The credential this request is about, e.g. 'gmail_app_password'. A second request for the same name supersedes the first."
                    },
                    "service": {
                        "type": "string",
                        "description": "What the user knows this as, e.g. 'Gmail' or 'secure.examplebank.com'."
                    },
                    "url": {
                        "type": "string",
                        "description": "For a website login, the page URL. Supply it and the credential names are derived from it for you, so a mistyped name cannot leave the credential somewhere the browser will not look."
                    },
                    "reason": {
                        "type": "string",
                        "description": "One short phrase on why you need it, shown to the user, e.g. 'to search your inbox'."
                    },
                    "fields": {
                        "type": "array",
                        "description": "One entry per value needed.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "key": {
                                    "type": "string",
                                    "description": "Credential name this answer is stored under."
                                },
                                "label": {
                                    "type": "string",
                                    "description": "What to show above the input, e.g. 'App password'."
                                },
                                "secret": {
                                    "type": "boolean",
                                    "default": true,
                                    "description": "Whether to mask the input. False for usernames and email addresses."
                                },
                                "hint": {
                                    "type": "string",
                                    "description": "Optional guidance, e.g. where to generate the value."
                                }
                            },
                            "required": ["key", "label"]
                        }
                    }
                },
                "required": ["name", "service", "fields"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let name = args["name"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing 'name' parameter".into()))?;
        let service = args["service"].as_str().map(|s| s.to_string());
        let reason = args["reason"].as_str().map(|s| s.to_string());

        let raw = args["fields"]
            .as_array()
            .ok_or_else(|| Error::ToolExecution("'fields' must be an array".into()))?;
        let mut fields = Vec::with_capacity(raw.len());
        for entry in raw {
            let key = entry["key"]
                .as_str()
                .ok_or_else(|| Error::ToolExecution("every field needs a 'key'".into()))?;
            let label = entry["label"]
                .as_str()
                .ok_or_else(|| Error::ToolExecution("every field needs a 'label'".into()))?;
            fields.push(RequestedField {
                key: key.to_string(),
                label: label.to_string(),
                // Masking is the default: a field the model forgot to
                // classify should err towards hidden, not towards shoulder-
                // surfable.
                secret: entry["secret"].as_bool().unwrap_or(true),
                hint: entry["hint"].as_str().map(|s| s.to_string()),
            });
        }
        // Repair website keys against the origin before anything is
        // stored under them.
        //
        // The agent authors these and has to reproduce them exactly twice
        // -- once here, once when the browser looks them up -- and it does
        // not reliably manage it. One observed run asked for
        // `web_m1_max_64_gb_..._password` while the browser derived
        // `web_m1_max_64gb_..._password`; a single underscore meant the
        // credential was stored where nothing would ever find it, the
        // lookup failed twelve times, and the login could not complete.
        // The same run spelled the username correctly, so nothing looked
        // broken from either side alone.
        //
        // `url` is preferred; `service` is used when it looks like a host,
        // because the agent supplies that already.
        let origin = args["url"]
            .as_str()
            .map(|u| u.to_string())
            .or_else(|| service.as_deref().and_then(crate::origin_from_service));
        if let Some(origin) = &origin {
            for f in fields.iter_mut() {
                if let Some(canonical) = crate::canonical_web_key(origin, &f.key) {
                    tracing::info!(
                        from = %f.key,
                        to = %canonical,
                        "repaired a website credential key to match the origin"
                    );
                    f.key = canonical;
                }
            }
        }

        if fields.is_empty() {
            return Err(Error::ToolExecution(
                "'fields' must name at least one value to ask for".into(),
            ));
        }

        // Redirect a request for a credential the application already
        // knows onto the name and fields it knows it by.
        //
        // The same freedom that makes this tool service-agnostic lets the
        // model file `gmail_credentials` beside the canonical
        // `gmail_app_password` -- observed 35 seconds apart for one
        // conversation, and on other days as `gmail_app_password_retry`
        // and `gmail_app_password_new_link`. `name` is the store's dedupe
        // key, so those do not collapse: the user is asked twice for one
        // password. Worse, fulfilling the invented request writes each
        // answer under the key the model chose, and a Gmail password
        // stored anywhere but `gmail_app_password` is a secret no tool
        // will ever read -- so the user types it and is asked again.
        //
        // See `known_credential`; website logins are left to the origin
        // repair above, which is the same fix against a different source
        // of truth.
        let (name, fields) =
            crate::known_credential::canonicalize(name, service.as_deref(), fields);
        let name = name.as_str();

        // The redirect gives this request the name the tools' own ask used,
        // so filing it now would supersede that ask -- and superseding kills
        // the link the user already has, which then opens the same "Link
        // expired" page a real expiry does. That is the sequence in the live
        // store every time: `gmail` asks, then the model asks again. So when
        // the known credential already has a live link, point at it instead,
        // exactly as `google_credentials::ask` does. The fields are the
        // canonical ones either way, so nothing the model asked for is lost.
        if crate::known_credential::is_known(name)
            && self.requests.has_live_link(name).await.unwrap_or(false)
        {
            let existing = self
                .requests
                .pending()
                .await
                .ok()
                .and_then(|rows| rows.into_iter().find(|r| r.name == name));
            if let Some(existing) = existing {
                let where_to_look = service.unwrap_or_else(|| name.to_string());
                return Ok(json!({
                    "status": "already_requested",
                    "request_id": existing.id,
                    "link_sent_separately": true,
                    "next_step": crate::credential_link::next_step_out_of_band(true, &where_to_look)
                }));
            }
        }

        // Which conversation is asking. This is what makes the answer
        // resumable: when the user supplies the value, this is the turn to
        // bring back. `None` outside a runner scope — the request is still
        // answerable in the app, it just cannot wake anything.
        let conversation_id = with_session_context(|c| c.conversation_id);

        let id = self
            .requests
            .file_fulfil(name, service.clone(), fields, reason, conversation_id)
            .await
            .map_err(|e| {
                Error::ToolExecution(format!("could not file the credential request: {e}").into())
            })?;

        let where_to_look = service.unwrap_or_else(|| name.to_string());

        // A link the user can open, when a base URL is configured. The
        // Apollo app can render this request from the pending list, but on
        // Telegram or Signal there is no app in the loop — a tappable URL
        // is the only way to hand someone a password field from a chat
        // message.
        // The link is minted here and handed to the deliverer, never to
        // the model. Asking a model to relay 64 hex characters verbatim
        // does not work reliably -- observed relaying 55 of 64, which the
        // user opens to a page that says "Link expired" and cannot
        // distinguish from a real timeout. Keeping it out of the result
        // also keeps it out of the transcript, where a live credential
        // capture URL would otherwise sit for as long as it works.
        let link = crate::credential_link::mint_link(&self.requests, &id).await;
        let queued = match (&self.pending_links, conversation_id, &link) {
            (Some(pending), Some(conv), Some(url)) => {
                pending.push(conv, url.clone());
                true
            }
            _ => false,
        };

        let next_step = crate::credential_link::next_step_out_of_band(queued, &where_to_look);

        Ok(json!({
            "status": "requested",
            "request_id": id,
            // Deliberately no link. Reporting whether one is on its way is
            // enough for the model to describe what happens next.
            "link_sent_separately": queued,
            // Phrased for the model's next turn: tell the user and stop,
            // do not poll, and do not carry on as if it had the value.
            "next_step": next_step
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::google_credentials::{KEY_APP_PASSWORD, KEY_EMAIL};

    /// A store in a tempdir with the keychain replaced, so a test cannot
    /// prompt for access or leave a secret on the developer's machine.
    fn test_store() -> (tempfile::TempDir, rustykrab_store::Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = rustykrab_store::Store::open(dir.path(), vec![7u8; 32])
            .expect("open store")
            .with_credential_backend(std::sync::Arc::new(
                rustykrab_store::credential_backend::MemoryBackend::new(),
            ));
        (dir, store)
    }

    fn request(name: &str, service: &str, keys: &[&str]) -> Value {
        json!({
            "name": name,
            "service": service,
            "reason": "to search your inbox",
            "fields": keys.iter().map(|k| json!({"key": k, "label": k})).collect::<Vec<_>>(),
        })
    }

    fn pending_keys(request: &rustykrab_store::CredentialRequest) -> Vec<&str> {
        request.fields.iter().map(|f| f.key.as_str()).collect()
    }

    /// The live failure: the tools' own ask files under `gmail_app_password`
    /// and the model files `gmail_credentials` beside it. `name` is the
    /// store's dedupe key, so the two rows both stayed pending and the user
    /// was prompted twice for one password.
    #[tokio::test]
    async fn an_invented_name_dedupes_against_the_canonical_ask() {
        let (_dir, store) = test_store();
        let requests = store.credential_requests();

        crate::google_credentials::ask(Some(&requests), None, "Gmail", "your app password").await;
        CredentialRequestTool::new(requests.clone())
            .execute(request(
                "gmail_credentials",
                "Gmail",
                &[KEY_EMAIL, KEY_APP_PASSWORD],
            ))
            .await
            .expect("filing should succeed");

        let pending = requests.pending().await.unwrap();
        assert_eq!(
            pending.len(),
            1,
            "one credential must produce one prompt, got {pending:?}"
        );
        assert_eq!(pending[0].name, KEY_APP_PASSWORD);
    }

    /// The other half: fulfilling a request writes each answer under the
    /// field key it names, so keys the model invented store the password
    /// where no tool looks and the user is asked again next turn.
    #[tokio::test]
    async fn invented_field_keys_are_filed_under_the_ones_the_tools_read() {
        let (_dir, store) = test_store();
        let requests = store.credential_requests();

        CredentialRequestTool::new(requests.clone())
            .execute(request(
                "gmail_credentials",
                "Gmail",
                &["gmail_username", "gmail_password"],
            ))
            .await
            .expect("filing should succeed");

        let pending = requests.pending().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].name, KEY_APP_PASSWORD);
        assert_eq!(pending_keys(&pending[0]), vec![KEY_EMAIL, KEY_APP_PASSWORD]);

        // End to end: answering the prompt leaves the credential where
        // `google_credentials::load` reads it.
        requests
            .fulfil(
                &pending[0].id,
                &[
                    (KEY_EMAIL.to_string(), "me@gmail.com".to_string()),
                    (KEY_APP_PASSWORD.to_string(), "abcdefghijklmnop".to_string()),
                ],
                "test",
            )
            .await
            .unwrap();
        let (email, password) =
            crate::google_credentials::load(&store.guarded_secrets(), None, None, "Gmail")
                .await
                .expect("the answer must land where Gmail reads it");
        assert_eq!(email, "me@gmail.com");
        assert_eq!(password, "abcdefghijklmnop");
    }

    /// The redirect must not cost the user the link they already have. The
    /// tools' own ask files and links first; the model's request, now under
    /// the same name, would otherwise supersede it and turn that link into
    /// "Link expired" while a second one is sent.
    #[tokio::test]
    async fn a_live_link_from_the_canonical_ask_is_kept() {
        let (_dir, store) = test_store();
        let requests = store.credential_requests();

        crate::google_credentials::ask(Some(&requests), None, "Gmail", "your app password").await;
        let asked = requests.pending().await.unwrap();
        let token = requests
            .issue_link(&asked[0].id, crate::credential_link::LINK_TTL)
            .await
            .unwrap();

        let out = CredentialRequestTool::new(requests.clone())
            .execute(request(
                "gmail_credentials",
                "Gmail",
                &[KEY_EMAIL, KEY_APP_PASSWORD],
            ))
            .await
            .expect("pointing at the existing ask should succeed");

        assert_eq!(out["status"], "already_requested");
        assert_eq!(out["request_id"], asked[0].id.as_str());
        let pending = requests.pending().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].id, asked[0].id,
            "the canonical ask was superseded"
        );
        assert!(
            requests.find_by_link(&token).await.unwrap().is_some(),
            "the link the user already has must still open"
        );
    }

    /// The service-name repair must not reach a website login: those are
    /// canonicalised against the origin instead, and a host that merely
    /// mentions a known service is still that host's own credential.
    #[tokio::test]
    async fn a_website_login_keeps_its_origin_derived_keys() {
        let (_dir, store) = test_store();
        let requests = store.credential_requests();
        let expected =
            crate::origin_credential_key("https://mail.google.com", crate::PASSWORD).unwrap();

        CredentialRequestTool::new(requests.clone())
            .execute(json!({
                "name": "web_mail_google_com_password",
                "service": "mail.google.com",
                "url": "https://mail.google.com/login",
                "fields": [{"key": "web_mail_google_com_password", "label": "Password"}],
            }))
            .await
            .expect("filing should succeed");

        let pending = requests.pending().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].name, "web_mail_google_com_password");
        assert_eq!(pending_keys(&pending[0]), vec![expected.as_str()]);
    }

    /// A credential the application has never heard of is what this tool is
    /// for, and is filed exactly as the agent asked.
    #[tokio::test]
    async fn an_unknown_credential_is_filed_as_asked() {
        let (_dir, store) = test_store();
        let requests = store.credential_requests();

        CredentialRequestTool::new(requests.clone())
            .execute(request(
                "acme_vpn_password",
                "ACME VPN",
                &["acme_vpn_password"],
            ))
            .await
            .expect("filing should succeed");

        let pending = requests.pending().await.unwrap();
        assert_eq!(pending[0].name, "acme_vpn_password");
        assert_eq!(pending_keys(&pending[0]), vec!["acme_vpn_password"]);
    }
}
