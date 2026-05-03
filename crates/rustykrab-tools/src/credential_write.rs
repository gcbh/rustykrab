use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Error, Result, Tool};
use rustykrab_store::SecretStore;
use serde_json::{json, Value};

/// A tool that writes credentials to the encrypted SecretStore or macOS
/// Keychain.
///
/// Supports storing, updating, and deleting secrets. The `import_from_keychain`
/// action copies a credential from the macOS Keychain into the encrypted local
/// store so it is available even when not running on macOS.
pub struct CredentialWriteTool {
    secrets: SecretStore,
}

impl CredentialWriteTool {
    pub fn new(secrets: SecretStore) -> Self {
        Self { secrets }
    }
}

#[async_trait]
impl Tool for CredentialWriteTool {
    fn name(&self) -> &str {
        "credential_write"
    }

    fn description(&self) -> &str {
        "Store, update, or delete a credential/secret. Use this to save API keys, \
         passwords, or tokens so they can be retrieved later with credential_read. \
         All credentials are encrypted at rest.\n\n\
         Required: 'action' and 'name'. 'value' is required for 'set'. 'source' \
         defaults to 'store' (the encrypted local store) — omit it for normal usage. \
         'service' and 'account' are required ONLY when source is 'keychain' or when \
         using 'import_from_keychain'; do not pass them (or pass empty strings) for \
         the local store.\n\n\
         Examples:\n\
         - Store locally: {\"action\": \"set\", \"name\": \"my_api_key\", \"value\": \"sk-123\"}\n\
         - Delete locally: {\"action\": \"delete\", \"name\": \"my_api_key\"}\n\
         - Store in keychain: {\"action\": \"set\", \"name\": \"deploy_key\", \"value\": \"sk-123\", \
         \"source\": \"keychain\", \"service\": \"myapp\", \"account\": \"deploy_token\"}\n\
         - Import from keychain: {\"action\": \"import_from_keychain\", \"name\": \"local_copy\", \
         \"service\": \"myapp\", \"account\": \"deploy_token\"}"
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
                        "enum": ["set", "delete", "import_from_keychain"],
                        "description": "Action: 'set' stores/updates, 'delete' removes, 'import_from_keychain' copies a macOS Keychain credential into the local store"
                    },
                    "name": {
                        "type": "string",
                        "description": "The name/key for the secret in the local store"
                    },
                    "value": {
                        "type": "string",
                        "description": "The secret value to store (required for 'set' action)"
                    },
                    "source": {
                        "type": "string",
                        "enum": ["store", "keychain"],
                        "default": "store",
                        "description": "Where to write: 'store' (default, encrypted local store) or 'keychain' (macOS Keychain). Omit to use 'store'."
                    },
                    "service": {
                        "type": "string",
                        "description": "macOS Keychain service name. Required ONLY when source is 'keychain' or action is 'import_from_keychain'; omit for source 'store'."
                    },
                    "account": {
                        "type": "string",
                        "description": "macOS Keychain account name (e.g. 'deploy_token', 'api_key'). Required ONLY when source is 'keychain' or action is 'import_from_keychain'; omit for source 'store'."
                    }
                },
                "required": ["action", "name"],
                "allOf": [
                    {
                        "if": { "properties": { "source": { "const": "keychain" } }, "required": ["source"] },
                        "then": { "required": ["service", "account"] }
                    },
                    {
                        "if": { "properties": { "action": { "const": "import_from_keychain" } }, "required": ["action"] },
                        "then": { "required": ["service", "account"] }
                    },
                    {
                        "if": { "properties": { "action": { "const": "set" } }, "required": ["action"] },
                        "then": { "required": ["value"] }
                    }
                ]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing action".into()))?;
        let name = args["name"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing name".into()))?;

        if action == "import_from_keychain" {
            return self.import_from_keychain(name, &args).await;
        }

        let source = args["source"].as_str().unwrap_or("store");

        match source {
            "keychain" => self.execute_keychain(action, name, &args).await,
            _ => self.execute_store(action, name, &args).await,
        }
    }
}

impl CredentialWriteTool {
    /// Write to the encrypted local SecretStore.
    async fn execute_store(&self, action: &str, name: &str, args: &Value) -> Result<Value> {
        match action {
            "set" => {
                let value = args["value"]
                    .as_str()
                    .ok_or_else(|| Error::ToolExecution("missing value for 'set' action".into()))?;

                self.secrets.set(name, value).map_err(|e| {
                    Error::ToolExecution(format!("failed to store secret: {e}").into())
                })?;

                Ok(json!({
                    "status": "stored",
                    "source": "store",
                    "name": name,
                }))
            }
            "delete" => {
                self.secrets.delete(name).map_err(|e| {
                    Error::ToolExecution(format!("failed to delete secret: {e}").into())
                })?;

                Ok(json!({
                    "status": "deleted",
                    "source": "store",
                    "name": name,
                }))
            }
            other => Err(Error::ToolExecution(
                format!(
                    "unknown action '{other}', expected 'set', 'delete', or 'import_from_keychain'"
                )
                .into(),
            )),
        }
    }

    /// Write to the macOS Keychain.
    async fn execute_keychain(&self, action: &str, name: &str, args: &Value) -> Result<Value> {
        if !rustykrab_store::keychain::keychain_available() {
            return Ok(json!({
                "error": "macOS Keychain is not available on this platform",
                "hint": "Use source 'store' to write to the encrypted local store instead",
            }));
        }

        let service = args["service"].as_str().ok_or_else(|| {
            Error::ToolExecution(
                "missing 'service' parameter. Provide the macOS Keychain service \
                 name. Example: {\"action\": \"set\", \"name\": \"key\", \"value\": \"val\", \
                 \"source\": \"keychain\", \"service\": \"myapp\", \"account\": \"deploy_token\"}"
                    .into(),
            )
        })?;
        let account = args["account"].as_str().ok_or_else(|| {
            Error::ToolExecution(
                "missing 'account' parameter. Provide the macOS Keychain account \
                 name (e.g. 'deploy_token', 'api_key'). Example: {\"action\": \"set\", \
                 \"name\": \"key\", \"value\": \"val\", \"source\": \"keychain\", \
                 \"service\": \"myapp\", \"account\": \"deploy_token\"}"
                    .into(),
            )
        })?;

        match action {
            "set" => {
                let value = args["value"]
                    .as_str()
                    .ok_or_else(|| Error::ToolExecution("missing value for 'set' action".into()))?;

                rustykrab_store::keychain::set_credential(service, account, value).map_err(
                    |e| Error::ToolExecution(format!("failed to store in keychain: {e}").into()),
                )?;

                tracing::info!(
                    service = service,
                    account = account,
                    "credential stored in macOS Keychain"
                );

                Ok(json!({
                    "status": "stored",
                    "source": "keychain",
                    "name": name,
                    "service": service,
                    "account": account,
                }))
            }
            "delete" => {
                rustykrab_store::keychain::delete_credential(service, account).map_err(|e| {
                    Error::ToolExecution(format!("failed to delete from keychain: {e}").into())
                })?;

                Ok(json!({
                    "status": "deleted",
                    "source": "keychain",
                    "service": service,
                    "account": account,
                }))
            }
            other => Err(Error::ToolExecution(
                format!("unknown action '{other}' for keychain source").into(),
            )),
        }
    }

    /// Import a credential from the macOS Keychain into the local encrypted
    /// store. This is the key operation for remote deployment: pull credentials
    /// from Keychain on a macOS dev machine and persist them in the portable
    /// encrypted store that travels with the deployment.
    async fn import_from_keychain(&self, name: &str, args: &Value) -> Result<Value> {
        if !rustykrab_store::keychain::keychain_available() {
            return Ok(json!({
                "error": "macOS Keychain is not available on this platform",
                "hint": "import_from_keychain must be run on macOS where the Keychain is accessible",
            }));
        }

        let service = args["service"].as_str().ok_or_else(|| {
            Error::ToolExecution(
                "missing 'service' parameter. Provide the macOS Keychain service name \
                 to import from. Example: {\"action\": \"import_from_keychain\", \
                 \"name\": \"local_copy\", \"source\": \"keychain\", \"service\": \"myapp\", \
                 \"account\": \"deploy_token\"}"
                    .into(),
            )
        })?;
        let account = args["account"].as_str().ok_or_else(|| {
            Error::ToolExecution(
                "missing 'account' parameter. Provide the macOS Keychain account name \
                 to import from (e.g. 'deploy_token', 'api_key'). Example: \
                 {\"action\": \"import_from_keychain\", \"name\": \"local_copy\", \
                 \"source\": \"keychain\", \"service\": \"myapp\", \"account\": \"deploy_token\"}"
                    .into(),
            )
        })?;

        // Read from Keychain.
        let cred = rustykrab_store::keychain::get_credential(service, account)
            .map_err(|e| Error::ToolExecution(format!("keychain read failed: {e}").into()))?
            .ok_or_else(|| {
                Error::ToolExecution(
                    format!(
                        "no credential found in Keychain for service '{service}', account '{account}'"
                    )
                    .into(),
                )
            })?;

        // Write to local encrypted store.
        self.secrets.set(name, &cred.value).map_err(|e| {
            Error::ToolExecution(format!("failed to store imported secret: {e}").into())
        })?;

        tracing::info!(
            name = name,
            service = service,
            account = account,
            "credential imported from macOS Keychain into local store"
        );

        Ok(json!({
            "status": "imported",
            "name": name,
            "keychain_service": service,
            "keychain_account": account,
        }))
    }
}
