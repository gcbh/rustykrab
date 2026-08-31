//! The one Google credential, and the one way of asking for it.
//!
//! Gmail and CalDAV are two protocols over a single account: IMAP/SMTP and
//! Google's DAV endpoint both authenticate with the same address and the
//! same 16-character app password, stored under [`KEY_EMAIL`] and
//! [`KEY_APP_PASSWORD`]. Because the credential is shared, everything about
//! obtaining it has to be shared too — a fix to how Gmail asks is a fix to
//! how the calendar asks, and it cannot be that only by coincidence.
//!
//! Both tools previously kept their own copy of the key names, the field
//! spec and the request-filing code. That is exactly the arrangement in
//! which one side gets the whitespace-stripping and the clobbered-value
//! check and the other quietly does not, which is what had happened. So
//! [`load`] is the whole flow — read both values, ask the user when either
//! is missing or unusable, normalise what comes back — and the tools call
//! it rather than reimplementing it.

use rustykrab_core::{Error, Result};
use rustykrab_store::{CredentialRequestStore, GuardedSecrets, RequestedField};

/// SecretStore keys. Named for Gmail because that is where they came from;
/// they are the Google account credential, and the calendar reads exactly
/// these.
pub const KEY_EMAIL: &str = "gmail_email";
pub const KEY_APP_PASSWORD: &str = "gmail_app_password";

/// The fields a user has to fill in, in the order they should appear.
///
/// One definition, so the calendar prompt cannot end up asking for
/// something different from the mail prompt for the same secret.
pub fn fields() -> Vec<RequestedField> {
    vec![
        RequestedField {
            key: KEY_EMAIL.to_string(),
            label: "Google account address".to_string(),
            secret: false,
            hint: None,
        },
        RequestedField {
            key: KEY_APP_PASSWORD.to_string(),
            label: "App password".to_string(),
            // Not the account password: both IMAP/SMTP and CalDAV take a
            // 16-character app password, generated per application.
            secret: true,
            hint: Some(
                "Sign in to Google and generate an app password — Apollo can do this for you."
                    .to_string(),
            ),
        },
    ]
}

/// File a request for the Google credentials and describe it to the model.
///
/// `service` is what the user recognises — "Gmail", "Google Calendar" —
/// and only changes the wording. The request is filed under one `name` for
/// both, so the store's dedupe means a user already being asked for mail is
/// not asked a second time for the calendar: one prompt, one answer, both
/// tools working.
///
/// Filing is best-effort by design: if the store rejects it, the caller
/// still gets an error explaining the gap, because a failure to ask is not
/// a reason to pretend the credential exists.
pub async fn ask(requests: Option<&CredentialRequestStore>, service: &str, needs: &str) -> String {
    let Some(requests) = requests else {
        return String::new();
    };
    // This is the path that actually fires in practice — measured at 25/25
    // against `browser`'s 1/9 — so a request filed here without a
    // conversation would leave the common case unresumable.
    let conversation_id = rustykrab_core::active_tools::with_session_context(|c| c.conversation_id);

    match requests
        .file_fulfil(
            KEY_APP_PASSWORD,
            Some(service.to_string()),
            fields(),
            // Reads as one sentence for either service: "so it can use
            // your Gmail", "so it can use your Google Calendar".
            Some(format!("so it can use your {service} — it needs {needs}")),
            conversation_id,
        )
        .await
    {
        Ok(id) => {
            // These tools file most of the requests that ever get filed, so
            // without a link they are also what most often leaves the user
            // with "open the app" and no way to answer from the chat they
            // are actually in.
            let link = crate::credential_link::mint_link(requests, &id).await;
            format!(
                " {}",
                crate::credential_link::next_step(link.as_deref(), service)
            )
        }
        Err(e) => {
            tracing::warn!(error = %e, service, "could not file a Google credential request");
            String::new()
        }
    }
}

/// Read the Google address and app password, asking the user for whatever
/// is missing.
///
/// When either is absent this asks the *user* rather than instructing the
/// model to invent a `credential_write` call: the model has no password to
/// write, so that advice could only ever produce a fabricated value or a
/// dead end, and it leaked an internal tool name into whatever the user was
/// reading.
pub async fn load(
    secrets: &GuardedSecrets,
    requests: Option<&CredentialRequestStore>,
    service: &str,
) -> Result<(String, String)> {
    let email = secrets.get(KEY_EMAIL).await.ok();
    let password = secrets.get(KEY_APP_PASSWORD).await.ok();

    let (email, password) = match (email, password) {
        (Some(email), Some(password)) => (email, password),
        (email, password) => {
            // Two phrasings: one for the model's error, one shown to the
            // user in the app, where "gmail_app_password" would mean
            // nothing.
            let (missing, needed) = match (email.is_some(), password.is_some()) {
                (false, false) => (
                    "the Google account address and app password are missing",
                    "your Google account address and app password",
                ),
                (true, false) => ("the app password is missing", "your Google app password"),
                (false, true) => (
                    "the Google account address is missing",
                    "your Google account address",
                ),
                (true, true) => unreachable!("both present is handled above"),
            };
            let asked = ask(requests, service, needed).await;
            return Err(Error::ToolExecution(
                format!("{service} is not set up yet: {missing}.{asked}").into(),
            ));
        }
    };

    let email = email.trim().to_string();
    let password = normalize_app_password(&password);

    // A value that cannot authenticate is as good as absent, so it takes
    // the same route: ask for a real one. Fulfilling the request overwrites
    // what is stored, so this is not a dead end.
    match validate(&email, &password) {
        Ok(()) => {}
        // Unwrapped rather than displayed: `{e}` would drop a "tool
        // execution error:" prefix into the middle of the sentence the ask
        // appends to.
        Err(Error::ToolExecution(reason)) => {
            let asked = ask(
                requests,
                service,
                "a working Google account address and app password",
            )
            .await;
            return Err(Error::ToolExecution(format!("{reason}{asked}").into()));
        }
        Err(other) => return Err(other),
    }

    Ok((email, password))
}

/// Remove all whitespace from a Google app password.
///
/// Google displays app passwords as four space-separated groups (`abcd efgh
/// ijkl mnop`). The spaces are not part of the secret: Gmail's IMAP and SMTP
/// tolerate them server-side, but a CalDAV `Basic` auth header base64-encodes
/// them verbatim and Google's DAV endpoint answers 401. Stripping here means
/// a password stored in the displayed format works everywhere, without asking
/// the user to type it again.
pub fn normalize_app_password(password: &str) -> String {
    password.replace(char::is_whitespace, "")
}

/// Reject stored credentials that cannot possibly authenticate, before any
/// request is built.
///
/// A mis-scoped `credential_write` can store a key's *name* as its own
/// value. That happened on the calendar side: from 2026-08-02 the store held
/// `gmail_email = "gmail_email"`, so every request addressed
/// `caldav/v2/gmail_email/...` and spent three retries earning a guaranteed
/// 401. An endpoint can only answer "unauthorized", which reads as a password
/// problem and sends debugging the wrong way — so name the real fault here,
/// for both protocols, since both read the same clobbered value.
pub fn validate(email: &str, password: &str) -> Result<()> {
    if email == KEY_EMAIL || !email.contains('@') {
        return Err(Error::ToolExecution(
            format!("stored {KEY_EMAIL} is not an email address, so it cannot authenticate.")
                .into(),
        ));
    }
    if password == KEY_APP_PASSWORD {
        return Err(Error::ToolExecution(
            format!(
                "stored {KEY_APP_PASSWORD} is the literal key name, not a password, \
                 so it cannot authenticate."
            )
            .into(),
        ));
    }
    Ok(())
}

/// Summarise a password's shape for an error message without revealing it.
///
/// Once the display spacing is stripped a Google app password is 16 ASCII
/// alphanumeric characters; anything else is worth surfacing when auth
/// fails. The value itself must never reach a log line, so this reports only
/// a length and a character class.
pub fn describe_password_shape(password: &str) -> String {
    let len = password.chars().count();
    if password.chars().all(|c| c.is_ascii_alphanumeric()) {
        format!("{len} alphanumeric characters (Google app passwords are 16)")
    } else {
        format!(
            "{len} characters including non-alphanumeric ones \
             (Google app passwords are 16 alphanumeric)"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Open a store in a tempdir the test keeps alive for its duration,
    /// keeping live values in memory.
    ///
    /// [`load`] reads through the credential backend before the database,
    /// and the real one on macOS is the login keychain: left on its default
    /// a test here would prompt for keychain access and hang, and a test
    /// that stored something would leave a password behind on the
    /// developer's machine.
    fn test_store() -> (tempfile::TempDir, rustykrab_store::Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = rustykrab_store::Store::open(dir.path(), vec![7u8; 32])
            .expect("open store")
            .with_credential_backend(std::sync::Arc::new(
                rustykrab_store::credential_backend::MemoryBackend::new(),
            ));
        (dir, store)
    }

    #[tokio::test]
    async fn missing_credentials_file_a_request_the_user_can_answer() {
        let (_dir, store) = test_store();
        let requests = store.credential_requests();

        let err = load(&store.guarded_secrets(), Some(&requests), "Gmail")
            .await
            .unwrap_err()
            .to_string();

        let pending = requests.pending().await.unwrap();
        assert_eq!(pending.len(), 1, "expected one request, got {pending:?}");
        assert_eq!(pending[0].name, KEY_APP_PASSWORD);
        assert_eq!(pending[0].service.as_deref(), Some("Gmail"));
        let keys: Vec<&str> = pending[0].fields.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys, vec![KEY_EMAIL, KEY_APP_PASSWORD]);
        // The user is asked; the model is not told to invent a write.
        assert!(
            !err.contains("credential_write"),
            "error leaked a tool name: {err}"
        );
    }

    #[tokio::test]
    async fn mail_and_calendar_ask_once_between_them() {
        let (_dir, store) = test_store();
        let requests = store.credential_requests();
        let secrets = store.guarded_secrets();

        load(&secrets, Some(&requests), "Gmail").await.unwrap_err();
        load(&secrets, Some(&requests), "Google Calendar")
            .await
            .unwrap_err();

        // One credential, one prompt: the second tool to notice the gap
        // must not stack another password field on the user.
        let pending = requests.pending().await.unwrap();
        assert_eq!(pending.len(), 1, "expected one request, got {pending:?}");
    }

    #[tokio::test]
    async fn answering_once_serves_both_protocols() {
        let (_dir, store) = test_store();
        let requests = store.credential_requests();
        let secrets = store.guarded_secrets();

        load(&secrets, Some(&requests), "Gmail").await.unwrap_err();
        let id = requests.pending().await.unwrap()[0].id.clone();
        requests
            .fulfil(
                &id,
                &[
                    (KEY_EMAIL.to_string(), "me@gmail.com".to_string()),
                    // As Google displays it, spaces and all.
                    (
                        KEY_APP_PASSWORD.to_string(),
                        "abcd efgh ijkl mnop".to_string(),
                    ),
                ],
                "test",
            )
            .await
            .unwrap();

        for service in ["Gmail", "Google Calendar"] {
            let (email, password) = load(&secrets, Some(&requests), service)
                .await
                .expect("credentials should be readable after one answer");
            assert_eq!(email, "me@gmail.com");
            // Stripped for both, not just for the tool that needs it most.
            assert_eq!(password, "abcdefghijklmnop");
        }
    }

    #[tokio::test]
    async fn a_stored_value_that_cannot_authenticate_asks_again() {
        let (_dir, store) = test_store();
        let requests = store.credential_requests();
        let secrets = store.secrets();
        // The exact shape the store held from 2026-08-02.
        secrets.create(KEY_EMAIL, KEY_EMAIL).await.unwrap();
        secrets
            .create(KEY_APP_PASSWORD, "abcdefghijklmnop")
            .await
            .unwrap();

        let err = load(&store.guarded_secrets(), Some(&requests), "Google Calendar")
            .await
            .unwrap_err()
            .to_string();

        assert!(err.contains("not an email address"), "{err}");
        assert_eq!(requests.pending().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn without_a_request_store_it_still_reports_the_gap() {
        let (_dir, store) = test_store();
        let err = load(&store.guarded_secrets(), None, "Gmail")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not set up yet"), "{err}");
    }

    #[test]
    fn clobbered_credentials_are_rejected_before_any_request() {
        let err = validate("gmail_email", "abcdefghijklmnop").unwrap_err();
        assert!(err.to_string().contains("not an email address"));

        let err = validate("me@gmail.com", "gmail_app_password").unwrap_err();
        assert!(err.to_string().contains("not a password"));
    }

    #[test]
    fn a_value_missing_an_at_sign_is_rejected() {
        assert!(validate("not-an-email", "abcdefghijklmnop").is_err());
    }

    #[test]
    fn well_formed_credentials_pass() {
        // A wrong-but-plausible password still passes here — only the
        // endpoint can judge that, and the 401 hint carries its shape.
        assert!(validate("me@gmail.com", "abcdefghijklmnop").is_ok());
        assert!(validate("me@gmail.com", "wrongbutplausible").is_ok());
    }

    #[test]
    fn app_password_whitespace_is_stripped() {
        assert_eq!(
            normalize_app_password("abcd efgh ijkl mnop"),
            "abcdefghijklmnop"
        );
        assert_eq!(
            normalize_app_password("  abcd\tefgh\nijkl mnop  "),
            "abcdefghijklmnop"
        );
        assert_eq!(
            normalize_app_password("abcdefghijklmnop"),
            "abcdefghijklmnop"
        );
    }

    #[test]
    fn password_shape_reports_length_and_class_but_never_the_value() {
        let secret = "abcdefghijklmnop";
        let described = describe_password_shape(secret);
        assert!(described.contains("16"));
        assert!(described.contains("alphanumeric"));
        // The point of the helper: a 401 message can carry the shape
        // without leaking the credential into a log line.
        assert!(!described.contains(secret));

        // A clobbered value reports its real shape so the mismatch is
        // visible without anyone having to read the stored secret.
        let wrong = describe_password_shape("gmail_app_password");
        assert!(wrong.contains("18"));
        assert!(wrong.contains("non-alphanumeric"));
        assert!(!wrong.contains("gmail_app_password"));
    }

    #[test]
    fn password_shape_counts_characters_not_bytes() {
        // A multi-byte character must count once, so the reported length
        // stays comparable to Google's 16.
        let described = describe_password_shape("é");
        assert!(described.contains('1'));
        assert!(!described.contains("2 characters"));
    }
}
