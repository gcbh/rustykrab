//! The agent's view of credential storage.
//!
//! Every tool that can write a credential is handed a [`GuardedSecrets`]
//! instead of a [`SecretStore`], so the policy lives in the type the tools
//! hold rather than in a check each tool has to remember. Creating a new
//! credential works normally; replacing or deleting one files a request for
//! the user to decide.
//!
//! Reads are unchanged — the agent needs credentials to do its job, and
//! restricting reads is a separate question (plan §14).

use rustykrab_core::Error;
use uuid::Uuid;

use crate::credential_request::CredentialRequestStore;
use crate::secret::SecretStore;

/// What happened to an agent-initiated write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    /// The name was free; the credential now exists.
    Created,
    /// The name was taken, so the change is queued for the user.
    PendingApproval { request_id: String },
}

#[derive(Clone)]
pub struct GuardedSecrets {
    secrets: SecretStore,
    requests: CredentialRequestStore,
    /// Conversation the current tool call belongs to, recorded on any
    /// request filed so the user can see what prompted it.
    conversation_id: Option<Uuid>,
}

impl GuardedSecrets {
    pub(crate) fn new(secrets: SecretStore, requests: CredentialRequestStore) -> Self {
        Self {
            secrets,
            requests,
            conversation_id: None,
        }
    }

    /// Attribute anything filed through this handle to a conversation.
    pub fn for_conversation(&self, conversation_id: Uuid) -> Self {
        Self {
            secrets: self.secrets.clone(),
            requests: self.requests.clone(),
            conversation_id: Some(conversation_id),
        }
    }

    /// Store a credential, or queue the change if the name is taken.
    ///
    /// Returns [`WriteOutcome`] rather than failing, so `credential_write`
    /// can tell the user "waiting for your approval" as a normal result.
    pub async fn set(&self, name: &str, value: &str) -> Result<WriteOutcome, Error> {
        self.set_with_reason(name, value, None).await
    }

    pub async fn set_with_reason(
        &self,
        name: &str,
        value: &str,
        reason: Option<String>,
    ) -> Result<WriteOutcome, Error> {
        match self.secrets.create(name, value).await {
            Ok(()) => Ok(WriteOutcome::Created),
            Err(Error::AlreadyExists(_)) => {
                let request_id = self
                    .requests
                    .file_update(name, value, reason, self.conversation_id)
                    .await?;
                Ok(WriteOutcome::PendingApproval { request_id })
            }
            Err(other) => Err(other),
        }
    }

    /// Like [`set`](Self::set) but reports a queued change as an error.
    ///
    /// For the configure flows (Gmail, CalDAV, Obsidian) that have no place
    /// to put a "pending" result: [`Error::PendingApproval`] carries the
    /// request id and renders as a self-explanatory message.
    pub async fn set_strict(&self, name: &str, value: &str) -> Result<(), Error> {
        match self.set(name, value).await? {
            WriteOutcome::Created => Ok(()),
            WriteOutcome::PendingApproval { request_id } => Err(Error::PendingApproval {
                request_id,
                name: name.to_string(),
            }),
        }
    }

    /// Queue a deletion. Deleting is never immediate for the agent, even
    /// for a credential it created itself — by the time it asks, the user
    /// may be relying on it.
    pub async fn delete(&self, name: &str) -> Result<WriteOutcome, Error> {
        // Nothing to queue if the credential doesn't exist.
        if self.secrets.version_of(name).await?.is_none() {
            return Err(Error::NotFound(format!("secret '{name}'")));
        }
        let request_id = self
            .requests
            .file_delete(name, None, self.conversation_id)
            .await?;
        Ok(WriteOutcome::PendingApproval { request_id })
    }

    // -- reads pass straight through --------------------------------------

    /// Hardware first, then the encrypted store.
    ///
    /// `gmail` and `caldav` read through here, and a credential the user
    /// handed over now lives in the keychain rather than the database — so
    /// consulting only the database would find nothing and report the
    /// credential missing immediately after the user supplied it.
    pub async fn get(&self, name: &str) -> Result<String, Error> {
        if let Some(v) = self.secrets.get_hardware(name) {
            return Ok(v);
        }
        self.secrets.get(name).await
    }

    pub async fn list_names(&self) -> Result<Vec<String>, Error> {
        self.secrets.list_names().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;

    fn guarded() -> (tempfile::TempDir, GuardedSecrets, SecretStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path(), vec![9u8; 32]).expect("open");
        (dir, store.guarded_secrets(), store.secrets())
    }

    #[tokio::test]
    async fn new_names_are_created_outright() {
        let (_dir, guard, secrets) = guarded();
        let outcome = guard.set("fresh_token", "value").await.unwrap();
        assert_eq!(outcome, WriteOutcome::Created);
        assert_eq!(secrets.get("fresh_token").await.unwrap(), "value");
    }

    #[tokio::test]
    async fn overwriting_queues_and_leaves_the_value_alone() {
        let (_dir, guard, secrets) = guarded();
        secrets.create("held", "original").await.unwrap();

        let outcome = guard.set("held", "hijacked").await.unwrap();
        let request_id = match outcome {
            WriteOutcome::PendingApproval { request_id } => request_id,
            other => panic!("expected a pending request, got {other:?}"),
        };

        // The credential is untouched until the user decides.
        assert_eq!(secrets.get("held").await.unwrap(), "original");
        assert_eq!(secrets.version_of("held").await.unwrap(), Some(1));

        let pending = guard.requests.pending().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, request_id);
        assert_eq!(pending[0].name, "held");
    }

    #[tokio::test]
    async fn deleting_always_queues() {
        let (_dir, guard, secrets) = guarded();
        // Even a credential the agent created itself.
        guard.set("agents_own", "v").await.unwrap();

        let outcome = guard.delete("agents_own").await.unwrap();
        assert!(matches!(outcome, WriteOutcome::PendingApproval { .. }));
        assert_eq!(secrets.get("agents_own").await.unwrap(), "v");
    }

    #[tokio::test]
    async fn strict_mode_reports_a_queued_change_as_an_error() {
        let (_dir, guard, secrets) = guarded();
        secrets.create("configured", "original").await.unwrap();

        let result = guard.set_strict("configured", "new").await;
        match result {
            Err(Error::PendingApproval { name, request_id }) => {
                assert_eq!(name, "configured");
                assert!(!request_id.is_empty());
            }
            other => panic!("expected PendingApproval, got {other:?}"),
        }
        assert_eq!(secrets.get("configured").await.unwrap(), "original");
    }
}
