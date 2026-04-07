use async_trait::async_trait;
use openclaw_core::types::ToolSchema;
use openclaw_core::{Error, Result, Tool};
use openclaw_store::SecretStore;
use serde_json::{json, Value};

/// A tool that writes credentials to the encrypted SecretStore.
///
/// Supports storing, updating, and deleting secrets.
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
         All credentials are encrypted at rest."
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
                        "enum": ["set", "delete"],
                        "description": "Action: 'set' stores/updates a secret, 'delete' removes one"
                    },
                    "name": {
                        "type": "string",
                        "description": "The name/key for the secret"
                    },
                    "value": {
                        "type": "string",
                        "description": "The secret value to store (required for 'set' action)"
                    }
                },
                "required": ["action", "name"]
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

        match action {
            "set" => {
                let value = args["value"]
                    .as_str()
                    .ok_or_else(|| Error::ToolExecution("missing value for 'set' action".into()))?;

                self.secrets
                    .set(name, value)
                    .map_err(|e| Error::ToolExecution(format!("failed to store secret: {e}").into()))?;

                Ok(json!({
                    "status": "stored",
                    "name": name,
                }))
            }
            "delete" => {
                self.secrets
                    .delete(name)
                    .map_err(|e| Error::ToolExecution(format!("failed to delete secret: {e}").into()))?;

                Ok(json!({
                    "status": "deleted",
                    "name": name,
                }))
            }
            other => Err(Error::ToolExecution(format!(
                "unknown action '{other}', expected 'set' or 'delete'"
            ).into())),
        }
    }
}
