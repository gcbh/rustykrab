use async_trait::async_trait;
use openclaw_core::types::ToolSchema;
use openclaw_core::{Error, Result, Tool};
use openclaw_store::SecretStore;
use serde_json::{json, Value};

/// A tool that reads credentials from the encrypted SecretStore.
///
/// Supports retrieving a specific secret by name or listing all
/// stored secret names (without revealing values).
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
         with external services. Credentials are stored encrypted at rest."
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
                        "enum": ["get", "list"],
                        "description": "Action to perform: 'get' retrieves a specific secret, 'list' shows all secret names"
                    },
                    "name": {
                        "type": "string",
                        "description": "The name/key of the secret to retrieve (required for 'get' action)"
                    }
                },
                "required": ["action"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing action".into()))?;

        match action {
            "get" => {
                let name = args["name"]
                    .as_str()
                    .ok_or_else(|| Error::ToolExecution("missing name for 'get' action".into()))?;

                match self.secrets.get(name) {
                    Ok(value) => {
                        // Mask the secret value to prevent leaking full secrets
                        // into the conversation. The full value remains available
                        // in the SecretStore for tools that need it directly.
                        let masked = if value.len() > 8 {
                            format!("{}...{}", &value[..4], &value[value.len()-4..])
                        } else {
                            "*".repeat(value.len())
                        };
                        Ok(json!({
                            "name": name,
                            "value": masked,
                        }))
                    }
                    Err(openclaw_core::Error::NotFound(_)) => Ok(json!({
                        "error": format!("no secret found with name '{name}'"),
                        "hint": "Use action 'list' to see available secret names",
                    })),
                    Err(e) => Err(Error::ToolExecution(format!("failed to read secret: {e}"))),
                }
            }
            "list" => {
                let names = self
                    .secrets
                    .list_names()
                    .map_err(|e| Error::ToolExecution(format!("failed to list secrets: {e}")))?;

                Ok(json!({
                    "secrets": names,
                    "count": names.len(),
                }))
            }
            other => Err(Error::ToolExecution(format!(
                "unknown action '{other}', expected 'get' or 'list'"
            ))),
        }
    }
}
