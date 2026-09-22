//! Credentials the application already has a name for.
//!
//! `credential_request` lets the model choose the request `name` and the
//! store key of every field. For a website login that freedom is necessary
//! — no registry entry can anticipate a site the user visits once — and
//! [`origin_key`](crate::origin_key) removes the guesswork by deriving
//! those keys from the URL.
//!
//! For a service the application already knows about it is pure downside,
//! and both halves of the downside were observed in the live store:
//!
//! 1. `name` is the store's dedupe key. The canonical Gmail ask files
//!    under [`KEY_APP_PASSWORD`]; the model filed `gmail_credentials`
//!    alongside it, and on other days `gmail_app_password_retry` and
//!    `gmail_app_password_new_link`. Different names do not dedupe, so the
//!    user was prompted twice for one password, each prompt carrying a
//!    link that superseding the other did not kill and expiry soon did.
//! 2. Fulfilling the invented request writes each answer under the field
//!    key the model chose. Anything other than `gmail_email` and
//!    `gmail_app_password` is a value no tool ever reads, so the user
//!    types the password, the store gains a secret, Gmail still fails, and
//!    the next turn asks again.
//!
//! So when a request names something the registry already knows, its name
//! and fields are replaced with the canonical ones before it is filed.
//! This is the same repair [`canonical_web_key`](crate::canonical_web_key)
//! performs for website keys, against the other source of truth: there the
//! origin, here [`rustykrab_store::registry`].
//!
//! [`KEY_APP_PASSWORD`]: crate::google_credentials::KEY_APP_PASSWORD

use rustykrab_store::RequestedField;

/// A credential this application resolves under a fixed name.
pub struct KnownCredential {
    /// The canonical request name — what the store dedupes on, and what
    /// the tools' own ask already uses.
    pub name: &'static str,
    /// Words that identify this credential in whatever the model typed.
    ///
    /// Matched against whole tokens, so `gmail_credentials` and "Google
    /// Calendar" both hit Google while `my-gmailer-app` does not.
    aliases: &'static [&'static str],
    /// Where the canonical field spec comes from.
    fields: Fields,
}

/// The source of a known credential's fields.
///
/// Neither variant carries labels of its own. A label written here would
/// be a second copy of one that already exists, free to drift from the
/// prompt the user actually sees — which is the class of bug this module
/// exists to close, not to reintroduce.
enum Fields {
    /// One value, labelled with its registry entry's own description.
    Registry(&'static str),
    /// The Google account address and app password, defined once in
    /// [`crate::google_credentials`] because Gmail and CalDAV share them.
    Google,
}

/// Every credential a request may be redirected onto.
///
/// Deliberately not the whole registry. `apns_auth_key` and
/// `rustykrab_auth_token` are operator configuration — one is a push
/// signing key read from a file, the other is generated when absent — and
/// an agent that asks a user to type either is confused about something
/// this table cannot fix.
pub static KNOWN: &[KnownCredential] = &[
    KnownCredential {
        name: crate::google_credentials::KEY_APP_PASSWORD,
        // "gmail" and "google" both appear because the one credential
        // serves mail and calendar, and the model names it after whichever
        // it happened to be using.
        aliases: &["gmail", "google", "googlemail", "caldav"],
        fields: Fields::Google,
    },
    KnownCredential {
        name: "notion_api_token",
        aliases: &["notion"],
        fields: Fields::Registry("notion_api_token"),
    },
    KnownCredential {
        name: "obsidian_api_key",
        aliases: &["obsidian"],
        fields: Fields::Registry("obsidian_api_key"),
    },
    KnownCredential {
        name: "anthropic_api_key",
        aliases: &["anthropic", "claude"],
        fields: Fields::Registry("anthropic_api_key"),
    },
];

impl KnownCredential {
    /// The fields the user should be asked for, in order.
    pub fn fields(&self) -> Vec<RequestedField> {
        match self.fields {
            Fields::Google => crate::google_credentials::fields(),
            Fields::Registry(key) => vec![RequestedField {
                key: key.to_string(),
                label: rustykrab_store::registry::lookup(key)
                    // Unreachable in practice and asserted by a test; a
                    // panic here would cost the user their prompt over a
                    // label, so the key stands in.
                    .map(|spec| spec.description.to_string())
                    .unwrap_or_else(|| key.to_string()),
                secret: true,
                hint: None,
            }],
        }
    }

    /// Whether a request already asks for exactly this, in this order.
    fn already_canonical(&self, name: &str, fields: &[RequestedField]) -> bool {
        name == self.name && fields == self.fields()
    }
}

/// The known credential a request is really about, if it is about one.
///
/// Matches on the request name, the service the user is shown, and the
/// field keys, because the model gets any subset of the three right: the
/// live store holds `gmail_credentials` (name wrong, keys right) and
/// `gmail_email` (name is a field of the credential, not the credential).
///
/// Returns `None` rather than guessing when the request could be about two
/// of them, and never touches a website login — those have their own
/// canonical form, derived from the origin, and rewriting one onto a named
/// service would store a site's password where a service expects its own.
pub fn canonical_for(
    name: &str,
    service: Option<&str>,
    fields: &[RequestedField],
) -> Option<&'static KnownCredential> {
    if is_web_key(name) || fields.iter().any(|f| is_web_key(&f.key)) {
        return None;
    }

    let mut matched: Option<&'static KnownCredential> = None;
    for known in KNOWN {
        let hit = mentions(name, known.aliases)
            || service.is_some_and(|s| mentions(s, known.aliases))
            || fields.iter().any(|f| mentions(&f.key, known.aliases));
        if !hit {
            continue;
        }
        if matched.is_some() {
            // Two services in one request is not a request this can
            // repair. Filing it as the model wrote it is wrong, but
            // filing it as the wrong service is wrong *and* silent.
            return None;
        }
        matched = Some(known);
    }
    matched
}

/// Apply [`canonical_for`], returning what should actually be filed.
///
/// Logs the rewrite: the request the user is shown will not be the one the
/// model wrote, and a run where that mattered should be readable afterwards.
pub fn canonicalize(
    name: &str,
    service: Option<&str>,
    fields: Vec<RequestedField>,
) -> (String, Vec<RequestedField>) {
    let Some(known) = canonical_for(name, service, &fields) else {
        return (name.to_string(), fields);
    };
    if known.already_canonical(name, &fields) {
        return (name.to_string(), fields);
    }
    tracing::info!(
        from = %name,
        to = %known.name,
        from_keys = ?fields.iter().map(|f| f.key.as_str()).collect::<Vec<_>>(),
        "redirected a credential request onto the credential the application knows"
    );
    (known.name.to_string(), known.fields())
}

/// Whether `haystack` contains one of `aliases` as a whole word.
///
/// Word boundaries are every run of non-alphanumerics, so `gmail_app_password`,
/// "Google Calendar" and `GMAIL-EMAIL` all split the way a reader would
/// expect. Matching on whole tokens rather than substrings keeps
/// `notion` out of `promotional_api_key`.
fn mentions(haystack: &str, aliases: &[&str]) -> bool {
    haystack
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .any(|token| {
            let token = token.to_ascii_lowercase();
            aliases.contains(&token.as_str())
        })
}

/// Whether a key belongs to the origin-derived website namespace.
fn is_web_key(key: &str) -> bool {
    key.starts_with("web_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::google_credentials::{KEY_APP_PASSWORD, KEY_EMAIL};

    fn field(key: &str) -> RequestedField {
        RequestedField {
            key: key.to_string(),
            label: key.to_string(),
            secret: true,
            hint: None,
        }
    }

    fn keys(fields: &[RequestedField]) -> Vec<&str> {
        fields.iter().map(|f| f.key.as_str()).collect()
    }

    /// The table exists to point at the registry, so every name and key in
    /// it has to be one the registry actually resolves. A typo here would
    /// send the answer to a key nothing reads — the failure it prevents.
    #[test]
    fn every_canonical_name_and_key_is_a_registry_entry() {
        for known in KNOWN {
            assert!(
                rustykrab_store::registry::lookup(known.name).is_some(),
                "{} is not in the registry",
                known.name
            );
            for f in known.fields() {
                assert!(
                    rustykrab_store::registry::lookup(&f.key).is_some(),
                    "{} asks for {}, which is not in the registry",
                    known.name,
                    f.key
                );
                assert!(!f.label.is_empty(), "{} has an unlabelled field", f.key);
            }
        }
    }

    /// The exact pair found pending in the live store, an hour apart for
    /// one conversation: the tools' own ask, and the model's invention.
    #[test]
    fn the_invented_gmail_name_resolves_to_the_canonical_one() {
        for invented in [
            "gmail_credentials",
            "gmail_app_password_new_link",
            "gmail_app_password_retry",
            "gmail_email",
            "google_account_login",
        ] {
            let (name, fields) = canonicalize(
                invented,
                Some("Gmail"),
                vec![field("gmail_email"), field("gmail_app_password")],
            );
            assert_eq!(name, KEY_APP_PASSWORD, "{invented} was not redirected");
            assert_eq!(keys(&fields), vec![KEY_EMAIL, KEY_APP_PASSWORD]);
        }
    }

    /// Both halves of the bug: an invented name *and* keys no tool reads.
    #[test]
    fn invented_field_keys_are_replaced_with_the_ones_the_tools_read() {
        let (name, fields) = canonicalize(
            "gmail_credentials",
            Some("Gmail"),
            vec![field("gmail_username"), field("gmail_password")],
        );
        assert_eq!(name, KEY_APP_PASSWORD);
        assert_eq!(keys(&fields), vec![KEY_EMAIL, KEY_APP_PASSWORD]);
        // And the user sees the prompt the tools' own ask would have shown,
        // not two boxes labelled after the model's guess.
        assert_eq!(fields, crate::google_credentials::fields());
    }

    /// The calendar and mail share one credential, so either name reaches it.
    #[test]
    fn the_calendar_reaches_the_same_credential_as_mail() {
        let (name, _) = canonicalize(
            "caldav_password",
            Some("Google Calendar"),
            vec![field("caldav_user"), field("caldav_password")],
        );
        assert_eq!(name, KEY_APP_PASSWORD);
    }

    /// The service string alone is enough when the name says nothing.
    #[test]
    fn the_service_the_user_sees_is_matched_too() {
        let (name, _) = canonicalize("login", Some("Notion"), vec![field("token")]);
        assert_eq!(name, "notion_api_token");
    }

    /// A single-value credential is labelled from its registry description,
    /// so the prompt cannot drift from what the application calls it.
    #[test]
    fn a_single_value_credential_is_labelled_from_the_registry() {
        let (_, fields) = canonicalize("obsidian_key", Some("Obsidian"), vec![field("key")]);
        assert_eq!(keys(&fields), vec!["obsidian_api_key"]);
        assert_eq!(
            fields[0].label,
            rustykrab_store::registry::lookup("obsidian_api_key")
                .unwrap()
                .description
        );
        assert!(fields[0].secret);
    }

    /// The canonical ask must pass through untouched — this runs on every
    /// Gmail and CalDAV request, and a rewrite there would be a rewrite of
    /// the thing being rewritten to.
    #[test]
    fn the_canonical_ask_is_left_exactly_as_it_is() {
        let original = crate::google_credentials::fields();
        let (name, fields) = canonicalize(KEY_APP_PASSWORD, Some("Gmail"), original.clone());
        assert_eq!(name, KEY_APP_PASSWORD);
        assert_eq!(fields, original);
    }

    /// Website logins have their own canonical form, derived from the
    /// origin. Redirecting one onto a named service would file a site's
    /// password where a service expects its own — so the two repairs must
    /// not both fire, whatever the host happens to be called.
    #[test]
    fn a_website_login_is_never_redirected() {
        let key = crate::origin_credential_key("https://mail.google.com", crate::PASSWORD).unwrap();
        assert!(canonical_for(&key, Some("mail.google.com"), &[field(&key)]).is_none());
        assert!(canonical_for(
            "web_notion_so_password",
            Some("notion.so"),
            &[field("web_notion_so_username")]
        )
        .is_none());
    }

    /// A credential the application has never heard of is the case this
    /// tool exists for, and must be filed exactly as asked.
    #[test]
    fn an_unknown_service_is_left_alone() {
        for (name, service) in [
            ("booking_details", "Audra Cutler Booking"),
            ("acme_vpn_password", "ACME VPN"),
        ] {
            let fields = vec![field("some_key")];
            let (got_name, got_fields) = canonicalize(name, Some(service), fields.clone());
            assert_eq!(got_name, name);
            assert_eq!(got_fields, fields);
        }
    }

    /// Whole tokens, not substrings: otherwise a credential whose name
    /// merely contains an alias would be swallowed by it.
    #[test]
    fn an_alias_inside_a_longer_word_does_not_match() {
        assert!(canonical_for("promotional_api_key", Some("Promotions"), &[]).is_none());
        assert!(canonical_for("gmailer_token", Some("Gmailer"), &[]).is_none());
    }

    /// Two services in one request is not something this can repair, and
    /// picking one would be a silent wrong answer.
    #[test]
    fn an_ambiguous_request_is_not_guessed_at() {
        assert!(canonical_for(
            "notion_to_gmail_sync",
            None,
            &[field("notion_api_token"), field("gmail_app_password")]
        )
        .is_none());
    }
}
