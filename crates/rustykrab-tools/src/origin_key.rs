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

/// The value a person types into the first box.
pub const USERNAME: &str = "username";
/// The value a person types into the second box.
pub const PASSWORD: &str = "password";

/// Store key for one field of a website login.
///
/// Keyed on host alone — not scheme, not port. A site reached over http
/// once and https thereafter, or on an explicit port, is the same login
/// to the person typing it; splitting the key would ask them for it
/// twice. `www.` is stripped for the same reason.
///
/// ```text
/// https://portal.example.com/login  + password -> web_portal_example_com_password
/// https://www.portal.example.com/   + username -> web_portal_example_com_username
/// ```
///
/// The result is restricted to the character set `SecretStore::validate_name`
/// accepts, so a host with a hyphen or an internationalised name cannot
/// produce a key the store will refuse.
pub fn origin_credential_key(url: &str, field: &str) -> Result<String> {
    let parsed = url::Url::parse(url).map_err(|e| {
        Error::ToolExecution(format!("cannot derive a credential key from '{url}': {e}").into())
    })?;
    let host = parsed.host_str().ok_or_else(|| {
        Error::ToolExecution(format!("'{url}' has no host, so there is no login to key on").into())
    })?;
    let host = host.strip_prefix("www.").unwrap_or(host);
    if host.is_empty() {
        return Err(Error::ToolExecution(
            format!("'{url}' has an empty host").into(),
        ));
    }

    let field = field.trim();
    if field.is_empty() {
        return Err(Error::ToolExecution(
            "a credential key needs a field name, e.g. 'username' or 'password'".into(),
        ));
    }

    let mut key = String::with_capacity(4 + host.len() + 1 + field.len());
    key.push_str("web_");
    push_sanitised(&mut key, host);
    key.push('_');
    push_sanitised(&mut key, field);
    Ok(key)
}

/// Lowercase ASCII alphanumerics survive; everything else becomes `_`.
///
/// Deliberately lossy and deliberately not reversible. The key only has
/// to be stable and legal, and collapsing punctuation means a host cannot
/// smuggle a character the store rejects — or, worse, one that reads as a
/// different key.
fn push_sanitised(out: &mut String, raw: &str) {
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push('_');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_site_always_yields_the_same_key() {
        // Different paths, different schemes, one login.
        let a = origin_credential_key("https://portal.example.com/login", PASSWORD).unwrap();
        let b = origin_credential_key("https://portal.example.com/account/2", PASSWORD).unwrap();
        let c = origin_credential_key("http://portal.example.com", PASSWORD).unwrap();
        assert_eq!(a, "web_portal_example_com_password");
        assert_eq!(a, b);
        assert_eq!(a, c, "scheme is not part of the identity of a login");
    }

    #[test]
    fn www_is_not_a_different_site() {
        assert_eq!(
            origin_credential_key("https://www.example.com/", USERNAME).unwrap(),
            origin_credential_key("https://example.com/", USERNAME).unwrap()
        );
    }

    #[test]
    fn username_and_password_are_separate_keys() {
        let u = origin_credential_key("https://example.com", USERNAME).unwrap();
        let p = origin_credential_key("https://example.com", PASSWORD).unwrap();
        assert_ne!(u, p);
        assert_eq!(u, "web_example_com_username");
        assert_eq!(p, "web_example_com_password");
    }

    #[test]
    fn a_port_does_not_split_the_login() {
        assert_eq!(
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
            got.as_deref(),
            Some("web_m1_max_64gb_tail84017e_ts_net_password")
        );
    }

    #[test]
    fn a_correct_key_is_left_alone() {
        assert_eq!(
            canonical_web_key(
                "https://m1-max-64gb.tail84017e.ts.net/demo/",
                "web_m1_max_64gb_tail84017e_ts_net_username"
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
