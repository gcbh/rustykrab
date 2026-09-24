//! Deterministic store keys for website logins.
//!
//! A website credential has no natural name the way `gmail_app_password`
//! does. Left to invent one, the agent picks a different string each time
//! — and `name` is the dedupe key on a credential request, so three
//! spellings of the same login become three pending asks and none of them
//! can be found again afterwards.
//!
//! Deriving the key from the URL removes the choice. The same site always
//! produces the same key, so the agent can file a request for a login it
//! has never seen and read it back later without anything recording a
//! mapping.

use rustykrab_core::{Error, Result};
use sha2::{Digest, Sha256};

/// The value a person types into the first box.
pub const USERNAME: &str = "username";
/// The value a person types into the second box.
pub const PASSWORD: &str = "password";

/// Store key for one field of a website login.
///
/// Version 2 hashes the canonical URL origin (scheme, exact host, effective
/// port), preserving browser security boundaries. Paths are not identity.
/// Old `web_host_with_underscores_*` keys and Instagram's named legacy keys
/// are intentionally not fallback candidates: their original origin cannot
/// be reconstructed without guessing. They remain stored; users re-enroll
/// through an origin-labelled secure form. No data is deleted or auto-migrated.
pub fn origin_credential_key(url: &str, field: &str) -> Result<String> {
    let origin = canonical_credential_origin(url)?;
    let field = field.trim();
    if !WEB_KEY_ROLES.contains(&field) {
        return Err(Error::ToolExecution(
            "a website credential key needs a supported login field, e.g. 'username' or 'password'"
                .into(),
        ));
    }
    Ok(format!(
        "web_v2_{}_{field}",
        hex::encode(Sha256::digest(origin.as_bytes()))
    ))
}

pub(crate) fn canonical_credential_origin(raw: &str) -> Result<String> {
    let parsed = url::Url::parse(raw).map_err(|_| {
        Error::ToolExecution("credential origin must be an absolute HTTP(S) URL".into())
    })?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(Error::ToolExecution(
            "credential origin must be an HTTP(S) URL without embedded credentials".into(),
        ));
    }
    Ok(parsed.origin().ascii_serialization())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_site_always_yields_the_same_key() {
        // Paths and an explicit default port are equivalent; schemes are not.
        let a = origin_credential_key("https://portal.example.com/login", PASSWORD).unwrap();
        let b = origin_credential_key("https://portal.example.com/account/2", PASSWORD).unwrap();
        let c = origin_credential_key("https://portal.example.com:443", PASSWORD).unwrap();
        assert!(a.starts_with("web_v2_"));
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn www_is_a_different_origin() {
        assert_ne!(
            origin_credential_key("https://www.example.com/", USERNAME).unwrap(),
            origin_credential_key("https://example.com/", USERNAME).unwrap()
        );
    }

    #[test]
    fn username_and_password_are_separate_keys() {
        let u = origin_credential_key("https://example.com", USERNAME).unwrap();
        let p = origin_credential_key("https://example.com", PASSWORD).unwrap();
        assert_ne!(u, p);
        assert!(u.ends_with("_username"));
        assert!(p.ends_with("_password"));
    }

    #[test]
    fn a_nondefault_port_is_a_different_origin() {
        assert_ne!(
            origin_credential_key("https://example.com:8443/login", PASSWORD).unwrap(),
            origin_credential_key("https://example.com/login", PASSWORD).unwrap()
        );
    }

    /// Every derived key must be storable. `SecretStore::validate_name`
    /// accepts only alphanumerics, `_`, `-` and `.`, so a host carrying
    /// anything else has to be collapsed rather than passed through.
    #[test]
    fn derived_keys_are_always_storable() {
        for url in [
            "https://my-bank.example.co.uk/login",
            "https://xn--bcher-kva.example/",
            "https://198.51.100.7:9000/",
            "https://a_b.example.com/",
        ] {
            let key = origin_credential_key(url, PASSWORD).unwrap();
            assert!(
                key.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.'),
                "{url} produced an unstorable key: {key}"
            );
            assert!(!key.is_empty());
        }
    }

    #[test]
    fn distinct_hosts_do_not_collide() {
        let a = origin_credential_key("https://a.example.com", PASSWORD).unwrap();
        let b = origin_credential_key("https://b.example.com", PASSWORD).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn a_url_with_no_host_is_refused() {
        assert!(origin_credential_key("not a url", PASSWORD).is_err());
        assert!(origin_credential_key("file:///etc/passwd", PASSWORD).is_err());
    }

    #[test]
    fn an_empty_field_is_refused() {
        assert!(origin_credential_key("https://example.com", "  ").is_err());
    }

    #[test]
    fn legacy_collisions_and_schemes_are_isolated() {
        let origins = [
            "https://a-b.example.com",
            "https://a.b.example.com",
            "http://a.b.example.com",
            "https://a_b.example.com",
            "https://www.a.b.example.com",
        ];
        let keys: std::collections::HashSet<_> = origins
            .into_iter()
            .map(|origin| origin_credential_key(origin, PASSWORD).unwrap())
            .collect();
        assert_eq!(keys.len(), origins.len());
        assert!(!keys.contains("web_a_b_example_com_password"));
    }
}

/// Field roles a website login key may end in.
///
/// Only these are canonicalised. A key ending in something else is left
/// alone: the point is to repair a mistyped *host*, not to second-guess
/// what the agent is asking for.
const WEB_KEY_ROLES: &[&str] = &["username", "password", "email", "totp", "otp", "pin"];

/// The role a web credential key encodes, if it encodes one.
///
/// `web_example_com_password` -> `password`. Callers that need to say
/// *how* a credential is used need the role rather than a guess: a key
/// may be a totp or a pin as easily as a password, and advice written
/// for a username/password pair is wrong for the rest.
pub fn role_of_web_key(key: &str) -> Option<&'static str> {
    if !key.starts_with("web_") {
        return None;
    }
    WEB_KEY_ROLES
        .iter()
        .find(|r| key.ends_with(&format!("_{r}")))
        .copied()
}

/// Rebuild a website credential key so its host matches what
/// [`origin_credential_key`] derives, keeping the role the agent chose.
///
/// The agent authors these keys and must reproduce them exactly twice —
/// once when asking, once when the browser looks them up. It does not
/// reliably manage that. Observed against gemma4:26b: it asked for
/// `web_m1_max_64_gb_..._password` and the browser derived
/// `web_m1_max_64gb_..._password`, one underscore apart, so the lookup
/// missed twelve times and the login could never complete. The same run
/// spelled the username key correctly, which is why this survived until
/// the two sides were compared directly.
///
/// Returns `None` when there is nothing to do: a key that is not a
/// website key, has no recognised role, or already matches.
pub fn canonical_web_key(origin: &str, key: &str) -> Option<String> {
    if !key.starts_with("web_") {
        return None;
    }
    let role = WEB_KEY_ROLES
        .iter()
        .find(|r| key.ends_with(&format!("_{r}")))?;
    let canonical = origin_credential_key(origin, role).ok()?;
    (canonical != key).then_some(canonical)
}

/// Best-effort URL for a `service` string the agent supplied.
///
/// `service` is meant to be what the user recognises — "Gmail",
/// "portal.example.com/login" — so it is only usable here when it looks
/// like a host. Anything without a dot is a product name, not an origin.
pub fn origin_from_service(service: &str) -> Option<String> {
    if service.trim().starts_with("https://") || service.trim().starts_with("http://") {
        return canonical_credential_origin(service.trim()).ok();
    }
    let s = service
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let host = s.split(['/', '?', '#']).next()?;
    (host.contains('.') && !host.contains(' ')).then(|| format!("https://{host}"))
}

#[cfg(test)]
mod canonical_tests {
    use super::*;

    /// The exact failure observed in a live run.
    #[test]
    fn a_mistyped_host_is_repaired() {
        let got = canonical_web_key(
            "https://m1-max-64gb.tail84017e.ts.net/demo/",
            "web_m1_max_64_gb_tail84017e_ts_net_password",
        );
        assert_eq!(
            got,
            Some(origin_credential_key("https://m1-max-64gb.tail84017e.ts.net", PASSWORD).unwrap())
        );
    }

    #[test]
    fn a_correct_key_is_left_alone() {
        assert_eq!(
            canonical_web_key(
                "https://m1-max-64gb.tail84017e.ts.net/demo/",
                &origin_credential_key("https://m1-max-64gb.tail84017e.ts.net", USERNAME).unwrap()
            ),
            None
        );
    }

    /// Service credentials are not website keys and must not be rewritten.
    #[test]
    fn a_named_service_credential_is_untouched() {
        assert_eq!(
            canonical_web_key("https://example.com", "gmail_app_password"),
            None
        );
        assert_eq!(
            canonical_web_key("https://example.com", "anthropic_api_key"),
            None
        );
    }

    /// An unrecognised role is left alone rather than guessed at.
    #[test]
    fn an_unknown_role_is_not_rewritten() {
        assert_eq!(
            canonical_web_key("https://example.com", "web_example_com_wibble"),
            None
        );
    }

    #[test]
    fn a_service_that_looks_like_a_host_yields_an_origin() {
        assert_eq!(
            origin_from_service("m1-max-64gb.tail84017e.ts.net/demo/").as_deref(),
            Some("https://m1-max-64gb.tail84017e.ts.net")
        );
        assert_eq!(
            origin_from_service("https://portal.example.com/login").as_deref(),
            Some("https://portal.example.com")
        );
    }

    /// A product name is not an origin.
    #[test]
    fn a_plain_service_name_yields_nothing() {
        assert_eq!(origin_from_service("Gmail"), None);
        assert_eq!(origin_from_service("my bank"), None);
    }
}
