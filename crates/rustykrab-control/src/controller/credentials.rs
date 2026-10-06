//! `needs_credential` on the credential page (plan section 7).
//!
//! A credential question that goes to the user files a `fulfil` request in
//! the credential registry and mints its one-time link, the path
//! `credential_request` takes (`credential_link`, `RUSTYKRAB_PUBLIC_URL`).
//! The link never enters the store: not the outbox body, not the question
//! row, not an event. Only its hash is written, by the request store. The
//! link waits in [`CredentialLinks`], in memory, until the notice that asks
//! the question is sent, and goes to the same channel as its own message
//! right after it. The item resumes on fulfilment: the credential is
//! stored, the catalog reports it, and the sweep answers the question and
//! requeues the item (`wake_on_credentials`).
//!
//! [`StoredCredentials`] is the catalog's side: the names the credential
//! registry holds, whatever path stored them, for `on_credential` triggers
//! and the wake.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use rustykrab_core::questions::QuestionKind;
use rustykrab_core::Error;
use rustykrab_store::{RequestedField, SecretStore};
use rustykrab_tools::credential_link::{self, LINK_TTL};
use rustykrab_tools::known_credential;

use super::batch::Batch;
use super::notice::{short_question, Cause};
use super::Controller;

/// What a credential question's notice can say about the credential page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CredentialPage {
    /// A request was filed and its link waits to follow the notice.
    Link,
    /// A link for the same credential is already out and still works.
    SentBefore,
    /// A request was filed; with no public URL it waits in the app.
    Filed,
}

/// Credential links minted for questions, held in memory until the notice
/// asking the question is sent. Keyed by question id; a notice names a
/// question by the first eight characters of it.
///
/// `Debug` shows how many links wait, never a link.
#[derive(Clone, Default)]
pub struct CredentialLinks {
    base: Option<String>,
    held: Arc<Mutex<Vec<Held>>>,
}

struct Held {
    question: String,
    url: String,
    at: Instant,
}

impl std::fmt::Debug for CredentialLinks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialLinks")
            .field("base", &self.base)
            .field("waiting", &self.waiting())
            .finish()
    }
}

impl CredentialLinks {
    /// Links on the public base URL the host is configured with
    /// (`RUSTYKRAB_PUBLIC_URL`), read once. With none, a credential question
    /// still files its request, which the app lists, and mints no link.
    pub fn from_env() -> Self {
        Self::with_base(credential_link::public_base())
    }

    /// Links on `base` (tests, and hosts that read the URL themselves).
    pub fn with_base(base: Option<String>) -> Self {
        CredentialLinks {
            base: base
                .map(|b| b.trim_end_matches('/').to_string())
                .filter(|b| !b.is_empty()),
            held: Arc::default(),
        }
    }

    fn held(&self) -> std::sync::MutexGuard<'_, Vec<Held>> {
        self.held.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Keep `url` for `question` until its notice is sent. A link older
    /// than its lifetime is dropped on the way: it no longer opens.
    pub fn hold(&self, question: &str, url: String) {
        let mut held = self.held();
        held.retain(|h| h.at.elapsed() < LINK_TTL && h.question != question);
        held.push(Held {
            question: question.to_string(),
            url,
            at: Instant::now(),
        });
    }

    /// The link minted for a question, by its id or the eight characters a
    /// notice shows, taken so it is sent once.
    pub fn take(&self, question: &str) -> Option<String> {
        let question = question.trim();
        if question.chars().count() < 8 {
            return None;
        }
        let mut held = self.held();
        let at = held
            .iter()
            .position(|h| h.question == question || h.question.starts_with(question))?;
        let h = held.remove(at);
        (h.at.elapsed() < LINK_TTL).then_some(h.url)
    }

    /// Every link a notice's questions carry, in the order the notice
    /// names them: what the channel sends after it, one message each.
    pub fn take_for_notice(&self, body: &str) -> Vec<String> {
        body.lines()
            .filter_map(|line| line.strip_prefix("Question "))
            .filter_map(|rest| rest.split_whitespace().next())
            .filter_map(|short| self.take(short))
            .collect()
    }

    /// How many links wait for their notice.
    pub fn waiting(&self) -> usize {
        self.held().len()
    }
}

/// The store name a credential is filed under: its name as the item gave
/// it, lowercased, with anything a secret name may not hold turned into `_`.
pub fn credential_key(name: &str) -> String {
    let mut key = String::new();
    for c in name.trim().chars() {
        let c = if c.is_ascii_alphanumeric() || matches!(c, '-' | '.') {
            c.to_ascii_lowercase()
        } else {
            '_'
        };
        if !(c == '_' && key.ends_with('_')) {
            key.push(c);
        }
    }
    let key: String = key.trim_matches('_').chars().take(128).collect();
    if key.is_empty() {
        "credential".to_string()
    } else {
        key
    }
}

fn field_for(key: &str, name: &str) -> RequestedField {
    RequestedField {
        key: key.to_string(),
        label: name.trim().to_string(),
        secret: true,
        hint: None,
    }
}

/// Every name the credential `name` may be stored under: as given, its
/// store key, and the canonical name of a credential the application
/// knows (a Gmail ask is filed as `gmail_app_password`).
pub fn stored_under(name: &str) -> Vec<String> {
    let key = credential_key(name);
    let known = known_credential::canonical_for(&key, Some(name), &[field_for(&key, name)])
        .map(|k| k.name.to_string());
    let mut names = vec![name.trim().to_string(), key];
    names.extend(known);
    names.dedup();
    names
}

/// The credential a question names, between backticks, as the controller
/// writes it (`... needs the credential `name``).
pub(super) fn credential_named(text: &str) -> Option<&str> {
    text.split('`')
        .nth(1)
        .map(str::trim)
        .filter(|n| !n.is_empty())
}

/// The credential names the registry holds, refreshed by the host, for a
/// [`super::ToolCatalog`]'s `credential_available`.
#[derive(Debug, Default)]
pub struct StoredCredentials {
    names: RwLock<HashSet<String>>,
}

impl StoredCredentials {
    pub fn new() -> Self {
        Self::default()
    }

    /// Re-read the registry: every name the secret store holds (the
    /// credential page, `credential_write`, the CLI and a migration each
    /// leave a row, even when the value lives in the keychain) and every
    /// registry entry whose environment override is set. Names only; no
    /// value is read. Returns how many names there are.
    pub async fn refresh(&self, secrets: &SecretStore) -> Result<usize, Error> {
        let mut names: HashSet<String> = secrets.list_names().await?.into_iter().collect();
        for spec in rustykrab_store::registry::REGISTRY {
            if std::env::var(spec.env_var).is_ok_and(|v| !v.trim().is_empty()) {
                names.insert(spec.store_name.to_string());
            }
        }
        let count = names.len();
        *self.names.write().unwrap_or_else(|e| e.into_inner()) = names;
        Ok(count)
    }

    /// Whether the credential `name` is stored under any name it may have.
    pub fn available(&self, name: &str) -> bool {
        let names = self.names.read().unwrap_or_else(|e| e.into_inner());
        stored_under(name).iter().any(|n| names.contains(n))
    }
}

impl Controller {
    /// The credential page for every credential question `b` sends
    /// (section 7): a `fulfil` request filed and its link minted and held
    /// for the notice, keyed by question id for the notice to say so. Only
    /// with [`Controller::with_credential_links`]; without it, a credential
    /// question asks the user to store it and answer.
    pub(super) async fn credential_pages(&self, b: &Batch) -> HashMap<String, CredentialPage> {
        let mut pages = HashMap::new();
        let Some(links) = &self.links else {
            return pages;
        };
        for (_, cause) in &b.causes {
            let Cause::Question {
                id,
                kind: QuestionKind::Credential,
                text,
                ..
            } = cause
            else {
                continue;
            };
            let Some(name) = credential_named(text) else {
                continue;
            };
            if let Some(page) = self.credential_page(links, id, name, text).await {
                pages.insert(id.clone(), page);
            }
        }
        pages
    }

    async fn credential_page(
        &self,
        links: &CredentialLinks,
        question: &str,
        name: &str,
        reason: &str,
    ) -> Option<CredentialPage> {
        let key = credential_key(name);
        let (filed_as, fields) =
            known_credential::canonicalize(&key, Some(name), vec![field_for(&key, name)]);
        let requests = self.store.credential_requests();
        // Filing again would supersede the request behind a link the user
        // already has, and that link would then open "Link expired".
        if requests.has_live_link(&filed_as).await.unwrap_or(false) {
            return Some(CredentialPage::SentBefore);
        }
        let id = match requests
            .file_fulfil(
                &filed_as,
                Some(name.to_string()),
                fields,
                Some(reason.to_string()),
                None,
            )
            .await
        {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(
                    question = %short_question(question),
                    error = %e,
                    "credential request not filed; the question asks without a page"
                );
                return None;
            }
        };
        Some(
            match credential_link::mint_link_at(&requests, &id, links.base.clone()).await {
                Some(url) => {
                    links.hold(question, url);
                    CredentialPage::Link
                }
                None => CredentialPage::Filed,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_credential_name_becomes_a_store_key() {
        assert_eq!(credential_key("bank login"), "bank_login");
        assert_eq!(
            credential_key("  Dentist Portal / main "),
            "dentist_portal_main"
        );
        assert_eq!(credential_key("carrier.api-token"), "carrier.api-token");
        assert_eq!(credential_key("???"), "credential");
    }

    #[test]
    fn a_known_credential_is_looked_for_under_its_canonical_name() {
        let names = stored_under("gmail");
        assert!(
            names.contains(&"gmail_app_password".to_string()),
            "{names:?}"
        );
        assert_eq!(stored_under("bank login"), vec!["bank login", "bank_login"]);
    }

    #[test]
    fn a_held_link_is_taken_once_by_the_short_id_a_notice_shows() {
        let links = CredentialLinks::with_base(Some("https://krab.example/".into()));
        links.hold("0123456789abcdef", "https://krab.example/c/token".into());
        assert_eq!(links.waiting(), 1);
        assert!(links.take("0123").is_none(), "too short to name a question");
        let body = "Asked: x\nQuestion 01234567 is for \"Pay\" (#aaaa).\nMore";
        assert_eq!(
            links.take_for_notice(body),
            vec!["https://krab.example/c/token".to_string()]
        );
        assert!(links.take_for_notice(body).is_empty(), "sent once");
        assert!(!format!("{links:?}").contains("token"));
    }
}
