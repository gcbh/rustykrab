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

                match self.secrets.get(name) {
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
                let names = self.secrets.list_names().map_err(|e| {
                    Error::ToolExecution(format!("failed to list secrets: {e}").into())
                })?;

                Ok(json!({
                    "source": "store",
                    "secrets": names,
                    "count": names.len(),
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
