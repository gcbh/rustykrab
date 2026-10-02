use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Error, Result, Tool};
use rustykrab_store::SecretStore;
use serde_json::{json, Value};

/// A tool that reads credentials from the encrypted SecretStore or from the
/// macOS Keychain.
///
/// Supports retrieving a specific secret by name or listing all stored secret
/// names (without revealing values). When `source` is set to `"keychain"`, the
/// tool reads directly from the macOS Keychain using a service/account pair —
/// useful for pulling deployment credentials (SSH keys, deploy tokens, API
/// keys) that are already stored in the system Keychain.
pub struct CredentialReadTool {
    secrets: SecretStore,
}

impl CredentialReadTool {
    pub fn new(secrets: SecretStore) -> Self {
        Self { secrets }
    }
}

#[async_trait]
impl Tool for CredentialReadTool {
    fn name(&self) -> &str {
        "credential_read"
    }

    fn description(&self) -> &str {
        "Read a stored credential/secret by name, or list all stored credential names. \
         Use this to retrieve API keys, passwords, or tokens needed to authenticate \
         with external services. Credentials are stored encrypted at rest.\n\n\
         Only 'action' is always required. 'source' defaults to 'store' (the encrypted \
         local store). 'name' is required for 'get'/'read' against the store. 'service' \
         and 'account' are required ONLY when source is 'keychain' — do not pass them \
         (or pass empty strings) for the local store.\n\n\
         Examples:\n\
         - List local secrets: {\"action\": \"list\"}\n\
         - Read from local store: {\"action\": \"get\", \"name\": \"my_api_key\"}\n\
         - Read from keychain: {\"action\": \"get\", \"source\": \"keychain\", \"service\": \"myapp\", \"account\": \"deploy_token\"}"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["get", "read", "list"],
                        "description": "Action to perform: 'get'/'read' retrieves a specific secret, 'list' shows all secret names"
                    },
                    "name": {
                        "type": "string",
                        "description": "The name/key of the secret to retrieve. Required for 'get'/'read' when source is 'store' (the default)."
                    },
                    "source": {
                        "type": "string",
                        "enum": ["store", "keychain"],
                        "default": "store",
                        "description": "Where to read from: 'store' (default, encrypted local store) or 'keychain' (macOS Keychain). Omit to use 'store'."
                    },
                    "service": {
                        "type": "string",
                        "description": "macOS Keychain service name (the 'Where' field in Keychain Access). Required ONLY when source is 'keychain'; omit for source 'store'."
                    },
                    "account": {
                        "type": "string",
                        "description": "macOS Keychain account name (e.g. 'deploy_token', 'api_key'). Required ONLY when source is 'keychain'; omit for source 'store'."
                    }
                },
                "required": ["action"],
                "allOf": [
                    {
                        "if": { "properties": { "source": { "const": "keychain" } }, "required": ["source"] },
                        "then": { "required": ["service", "account"] }
                    }
                ]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing action".into()))?;

        let source = args["source"].as_str().unwrap_or("store");

        match source {
            "keychain" => self.execute_keychain(action, &args).await,
            _ => self.execute_store(action, &args).await,
        }
    }
}

impl CredentialReadTool {
    /// Read credentials from the encrypted local SecretStore.
    async fn execute_store(&self, action: &str, args: &Value) -> Result<Value> {
        match action {
            "get" | "read" => {
                let name = args["name"]
                    .as_str()
                    .ok_or_else(|| Error::ToolExecution("missing name for 'get' action".into()))?;

                // A credential the user supplied through the secure form
                // lives in the OS credential store; the database keeps an
                // empty placeholder row so the name and version still
                // exist. Decrypting that row yields `""`, and returning it
                // as a value tells the agent it holds an empty password.
                //
                // Measured: the agent then submits the empty string
                // (`LOGIN FAILED user='' pw_len=0`) or invents one, and
                // re-reads the store ten to fourteen times a turn trying
                // to get something better. It is not being obtuse -- it
                // was told the read succeeded.
                //
                // So say what is true: it is stored, it is deliberately
                // not readable here, and here is how to use it.
                let held_in_backend = self.secrets.get_hardware(name).is_some();
                match self.secrets.get(name).await {
                    Ok(value) if value.is_empty() && held_in_backend => Ok(json!({
                        "source": "store",
                        "name": name,
                        "stored": true,
                        "readable": false,
                        "why": "This credential is held in the OS secure store. Its value is \
                                deliberately not returned here so it never enters the \
                                conversation.",
                        "how_to_use": how_to_use(name),
                    })),
                    Ok(value) => Ok(json!({
                        "source": "store",
                        "name": name,
                        "value": value,
                    })),
                    Err(rustykrab_core::Error::NotFound(_)) => Ok(json!({
                        "error": format!("no secret found with name '{name}'"),
                        "hint": "Use action 'list' to see available secret names, or try source 'keychain' to check the macOS Keychain",
                    })),
                    Err(e) => Err(Error::ToolExecution(
                        format!("failed to read secret: {e}").into(),
                    )),
                }
            }
            "list" => {
                let names = self.secrets.list_names().await.map_err(|e| {
                    Error::ToolExecution(format!("failed to list secrets: {e}").into())
                })?;

                // Split by whether the value can be read here. An agent
                // that sees a name and assumes it can fetch the value
                // wastes the turn discovering otherwise; one that is told
                // "you have this, use fill_credential" can act on it. This
                // is the case where the agent already *has* what it needs
                // and should simply proceed.
                let (held_in_backend, readable): (Vec<&String>, Vec<&String>) = names
                    .iter()
                    .partition(|n| self.secrets.get_hardware(n).is_some());

                Ok(json!({
                    "source": "store",
                    "secrets": names,
                    "count": names.len(),
                    "readable_here": readable,
                    "held_in_secure_store": held_in_backend,
                    "note": "Names under 'held_in_secure_store' are present and usable, but \
                             their values are deliberately not readable — do not call 'get' \
                             for them. Sign in with browser(action='fill_credential', \
                             ref=<ref>, field='username'|'password'), which reads them \
                             directly. You already have these; there is no need to ask the \
                             user for them again.",
                    "keychain_available": rustykrab_store::keychain::keychain_available(),
                }))
            }
            other => Err(Error::ToolExecution(
                format!("unknown action '{other}', expected 'get' or 'list'").into(),
            )),
        }
    }

    /// Read credentials from the macOS Keychain.
    async fn execute_keychain(&self, action: &str, args: &Value) -> Result<Value> {
        if !rustykrab_store::keychain::keychain_available() {
            return Ok(json!({
                "error": "macOS Keychain is not available on this platform",
                "hint": "Use source 'store' to read from the encrypted local store instead",
            }));
        }

        match action {
            "get" | "read" => {
                let service = args["service"].as_str().ok_or_else(|| {
                    Error::ToolExecution(
                        "missing 'service' parameter. Provide the macOS Keychain service \
                         name (the 'Where' field in Keychain Access). Example: \
                         {\"action\": \"get\", \"source\": \"keychain\", \"service\": \"myapp\", \
                         \"account\": \"deploy_token\"}"
                            .into(),
                    )
                })?;
                let account = args["account"].as_str().ok_or_else(|| {
                    Error::ToolExecution(
                        "missing 'account' parameter. Provide the macOS Keychain account \
                         name associated with the entry (e.g. 'deploy_token', 'api_key'). \
                         Example: {\"action\": \"get\", \"source\": \"keychain\", \
                         \"service\": \"myapp\", \"account\": \"deploy_token\"}"
                            .into(),
                    )
                })?;

                match rustykrab_store::keychain::get_credential(service, account) {
                    Ok(Some(cred)) => {
                        tracing::info!(
                            service = service,
                            account = account,
                            "credential retrieved from macOS Keychain"
                        );
                        Ok(json!({
                            "source": "keychain",
                            "service": cred.service,
                            "account": cred.account,
                            "value": cred.value,
                        }))
                    }
                    Ok(None) => Ok(json!({
                        "error": format!("no credential found in Keychain for service '{service}', account '{account}'"),
                        "hint": "Verify the service and account names in Keychain Access.app, or store the credential with credential_write using source 'keychain'",
                    })),
                    Err(e) => Err(Error::ToolExecution(
                        format!("keychain lookup failed: {e}").into(),
                    )),
                }
            }
            "list" => {
                // macOS Keychain does not provide a generic "list all items" API
                // via security-framework. Direct the user to use Keychain Access
                // or `security dump-keychain` for discovery.
                Ok(json!({
                    "source": "keychain",
                    "error": "listing all Keychain items is not supported via this tool",
                    "hint": "Use Keychain Access.app or `security dump-keychain` to discover service/account names, then use action 'get' with the specific service and account",
                }))
            }
            other => Err(Error::ToolExecution(
                format!("unknown action '{other}', expected 'get' or 'list'").into(),
            )),
        }
    }
}

/// How to use a credential whose value cannot be returned.
///
/// This used to prescribe a login: fill `username`, then `password`.
/// That is a recipe for one situation stated as if it were the only one.
/// The tool is asked about a single credential, and the roles include
/// `totp`, `otp`, `email` and `pin` -- for any of those, instructions to
/// fill a username and a password name two credentials that were not
/// asked about and omit the one that was. For a stored secret that is
/// not a web credential at all, "take a browser snapshot" is not merely
/// incomplete, it points somewhere there is nothing to do.
///
/// So: name the field this key actually encodes, and when the key
/// encodes nothing, say what is true and stop rather than invent a
/// procedure.
fn how_to_use(name: &str) -> String {
    let common = "Do not try to read it again — the answer will not change.";
    match crate::origin_key::role_of_web_key(name) {
        Some(role) => format!(
            "{common} To use it on a page, take a browser snapshot and call \
             browser(action='fill_credential', ref=<ref>, field='{role}'). That reads \
             '{name}' directly and types it into the page without showing it to you. \
             Other fields on the same form are separate credentials with their own \
             names; fill each one with its own call."
        ),
        None => format!(
            "{common} '{name}' is not a web credential, so there is no field to fill. \
             Tools that can use it read it from the store themselves; its value is \
             not available to you here."
        ),
    }
}

#[cfg(test)]
mod hardware_held_tests {
    use super::*;
    use rustykrab_store::{Store, WriteAuthority};

    /// A store whose credentials live in a backend, as they do after a
    /// user answers the secure form.
    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path(), vec![3u8; 32])
            .expect("open")
            .with_credential_backend(std::sync::Arc::new(
                rustykrab_store::credential_backend::MemoryBackend::new(),
            ));
        (dir, store)
    }

    /// The bug this exists for: `put_hardware` leaves an empty placeholder
    /// row, `get` decrypts it to `""`, and returning that as a value told
    /// the agent it held an empty password. Measured consequence was a
    /// submitted blank form and ten-plus re-reads per turn.
    #[tokio::test]
    async fn a_securely_held_credential_is_not_reported_as_an_empty_value() {
        let (_dir, store) = store();
        store
            .secrets()
            .put_hardware(
                "web_example_com_password",
                "hunter2",
                WriteAuthority::User { device: None },
            )
            .await
            .expect("put_hardware");

        let out = CredentialReadTool::new(store.secrets())
            .execute(json!({"action": "get", "name": "web_example_com_password"}))
            .await
            .expect("read");

        assert_eq!(
            out["value"],
            serde_json::Value::Null,
            "never return a value"
        );
        assert_eq!(out["stored"], true, "the agent must know it has this");
        assert_eq!(out["readable"], false);
        assert!(
            out["how_to_use"]
                .as_str()
                .unwrap()
                .contains("fill_credential"),
            "must point at the action that can actually use it: {out}"
        );
    }

    /// The value must not leak through the new shape either.
    #[tokio::test]
    async fn the_secret_never_appears_in_the_response() {
        let (_dir, store) = store();
        store
            .secrets()
            .put_hardware(
                "web_example_com_password",
                "hunter2",
                WriteAuthority::User { device: None },
            )
            .await
            .expect("put_hardware");

        let out = CredentialReadTool::new(store.secrets())
            .execute(json!({"action": "get", "name": "web_example_com_password"}))
            .await
            .expect("read");

        assert!(!out.to_string().contains("hunter2"), "leaked: {out}");
    }

    /// An ordinary stored secret still reads normally — the change must
    /// not break credentials that are meant to be readable.
    #[tokio::test]
    async fn an_ordinary_secret_still_returns_its_value() {
        let (_dir, store) = store();
        store
            .secrets()
            .create("plain_token", "abc123")
            .await
            .expect("create");

        let out = CredentialReadTool::new(store.secrets())
            .execute(json!({"action": "get", "name": "plain_token"}))
            .await
            .expect("read");

        assert_eq!(out["value"], "abc123");
    }

    /// Listing is where an agent decides whether it already has what it
    /// needs, so it has to distinguish "present and usable" from
    /// "present and fetchable".
    #[tokio::test]
    async fn list_separates_held_from_readable() {
        let (_dir, store) = store();
        store
            .secrets()
            .create("plain_token", "abc123")
            .await
            .expect("create");
        store
            .secrets()
            .put_hardware(
                "web_example_com_password",
                "hunter2",
                WriteAuthority::User { device: None },
            )
            .await
            .expect("put_hardware");

        let out = CredentialReadTool::new(store.secrets())
            .execute(json!({"action": "list"}))
            .await
            .expect("list");

        let held: Vec<&str> = out["held_in_secure_store"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        let readable: Vec<&str> = out["readable_here"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();

        assert_eq!(held, vec!["web_example_com_password"]);
        assert_eq!(readable, vec!["plain_token"]);
        assert!(!out.to_string().contains("hunter2"), "leaked: {out}");
    }

    /// The advice must name the field this key encodes -- not a login
    /// recipe that happens to be right for two of the six roles.
    #[test]
    fn advice_names_the_role_the_key_encodes() {
        let totp = super::how_to_use("web_example_com_totp");
        assert!(totp.contains("field='totp'"), "{totp}");
        assert!(
            !totp.contains("username") && !totp.contains("password"),
            "a totp is not a username/password pair: {totp}"
        );

        let pw = super::how_to_use("web_example_com_password");
        assert!(pw.contains("field='password'"), "{pw}");
        assert!(
            !pw.contains("field='username'"),
            "only the key asked about: {pw}"
        );
    }

    /// A stored secret that is not a web credential has no field to fill,
    /// and pointing at a browser would send the agent somewhere with
    /// nothing to do.
    #[test]
    fn non_web_secrets_get_no_browser_instructions() {
        let out = super::how_to_use("stripe_api_key");
        assert!(!out.contains("fill_credential"), "{out}");
        assert!(!out.contains("snapshot"), "{out}");
        assert!(out.contains("not a web credential"), "{out}");
    }

    /// Every role the key format supports must produce usable advice.
    #[test]
    fn every_supported_role_is_named() {
        for role in ["username", "password", "email", "totp", "otp", "pin"] {
            let out = super::how_to_use(&format!("web_example_com_{role}"));
            assert!(out.contains(&format!("field='{role}'")), "{role}: {out}");
        }
    }
}
