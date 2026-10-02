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
        // A backend that cannot be read is an error, not an absence. Falling
        // through to the database on a keychain failure is what turned a
        // locked machine into "your stored Gmail address is not an email
        // address": the row is there, and it is empty.
        if let Some(v) = self.secrets.try_get_hardware(name)? {
            return Ok(v);
        }
        match self.secrets.get(name).await {
            // `put_hardware` keeps the live value in the backend and leaves
            // an empty row behind, because the column is NOT NULL. That row
            // means "this one lives in hardware" — it is not a credential,
            // and handing `""` to a caller that is about to authenticate
            // with it only produces a puzzling error further downstream.
            Ok(v) if v.is_empty() => Err(Error::NotFound(format!("secret '{name}'"))),
            other => other,
        }
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

    /// A credential backend that can be made to fail reads, or to forget
    /// what it holds, without disturbing the database rows beside it.
    ///
    /// Both are states a real keychain reaches: denying reads while the
    /// machine is locked, and losing an item outright.
    #[derive(Default)]
    struct FlakyBackend {
        inner: crate::credential_backend::MemoryBackend,
        deny_reads: std::sync::atomic::AtomicBool,
        forget: std::sync::atomic::AtomicBool,
    }

    impl crate::credential_backend::CredentialBackend for FlakyBackend {
        fn name(&self) -> &str {
            "flaky (test)"
        }
        fn available(&self) -> bool {
            true
        }
        fn get(&self, account: &str) -> Result<Option<String>, Error> {
            use std::sync::atomic::Ordering;
            if self.deny_reads.load(Ordering::SeqCst) {
                // The message macOS actually produces when locked.
                return Err(Error::Storage(
                    "keychain read failed: User interaction is not allowed".to_string(),
                ));
            }
            if self.forget.load(Ordering::SeqCst) {
                return Ok(None);
            }
            crate::credential_backend::CredentialBackend::get(&self.inner, account)
        }
        fn set(&self, account: &str, value: &str) -> Result<(), Error> {
            crate::credential_backend::CredentialBackend::set(&self.inner, account, value)
        }
        fn delete(&self, account: &str) -> Result<(), Error> {
            crate::credential_backend::CredentialBackend::delete(&self.inner, account)
        }
    }

    /// A store whose credential backend is a [`FlakyBackend`], with a Gmail
    /// address already deposited in hardware the ordinary way.
    async fn with_flaky_backend() -> (
        tempfile::TempDir,
        GuardedSecrets,
        std::sync::Arc<FlakyBackend>,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = std::sync::Arc::new(FlakyBackend::default());
        let store = Store::open(dir.path(), vec![9u8; 32])
            .expect("open")
            .with_credential_backend(backend.clone());
        store
            .secrets()
            .put_hardware(
                "gmail_email",
                "me@gmail.com",
                crate::secret::WriteAuthority::User {
                    device: Some("device:test".to_string()),
                },
            )
            .await
            .expect("put_hardware");
        (dir, store.guarded_secrets(), backend)
    }

    #[tokio::test]
    async fn a_credential_store_that_cannot_be_read_is_an_error_not_an_empty_value() {
        let (_dir, guard, backend) = with_flaky_backend().await;
        assert_eq!(guard.get("gmail_email").await.unwrap(), "me@gmail.com");

        // The machine locks; the credential is untouched but unreachable.
        backend
            .deny_reads
            .store(true, std::sync::atomic::Ordering::SeqCst);

        // The bug this pins: falling through to the database returned the
        // empty placeholder row, so `gmail` reported the user's stored
        // address was "not an email address" and asked them to supply it
        // again — for four days, with a working credential in the keychain.
        match guard.get("gmail_email").await {
            Err(Error::Storage(msg)) => assert!(msg.contains("User interaction is not allowed")),
            other => panic!("expected the read failure to surface, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_hardware_backed_secret_the_backend_lost_reads_as_absent() {
        let (_dir, guard, backend) = with_flaky_backend().await;

        // The keychain answers, and genuinely no longer holds the item.
        backend
            .forget
            .store(true, std::sync::atomic::Ordering::SeqCst);

        // The empty row `put_hardware` leaves behind marks where the value
        // lives; it is not itself a credential. "Absent" is the honest
        // answer, and the one that makes asking the user the right move.
        match guard.get("gmail_email").await {
            Err(Error::NotFound(what)) => assert!(what.contains("gmail_email")),
            other => panic!("expected NotFound, got {other:?}"),
        }
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
