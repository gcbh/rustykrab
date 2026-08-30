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
}

impl CredentialRequestTool {
    pub fn new(requests: CredentialRequestStore) -> Self {
        Self { requests }
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
         this whole flow exists to avoid."
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
        if fields.is_empty() {
            return Err(Error::ToolExecution(
                "'fields' must name at least one value to ask for".into(),
            ));
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
        let link = crate::credential_link::mint_link(&self.requests, &id).await;
        let next_step = crate::credential_link::next_step(link.as_deref(), &where_to_look);

        Ok(json!({
            "status": "requested",
            "request_id": id,
            "link": link,
            // Phrased for the model's next turn: it should tell the user
            // and stop, not poll, and not carry on as if it had the value.
            "next_step": next_step
        }))
    }
}
