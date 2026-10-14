//! Secrets taken out of text before it is copied anywhere a worker reads.
//!
//! The daemon log is the case that matters: until the fix in
//! `rustykrab-channels/src/telegram.rs` it logged the Bot API URL of every
//! failed `getUpdates`, and that URL carries the bot token
//! (`https://api.telegram.org/bot<id>:<token>/getUpdates`). Logs written
//! before the fix still hold it, and an investigation bundle copies log
//! lines into a directory an agent CLI is pointed at.
//!
//! [`redact`] is a fixed, ordered set of scanners, no model and no
//! configuration, so the same line always redacts the same way:
//!
//! 1. a Bot API path, `/bot<digits>:<token>`, and a bare token of that
//!    shape;
//! 2. a `Bearer <credential>` or `Basic <credential>` header value;
//! 3. a key with a known prefix (`sk-`, `sk_live_`, `ghp_`, `github_pat_`,
//!    `xoxb-`, `AKIA`, ...);
//! 4. the value of a `token=`, `api_key=`, `password:`, `"secret": "..."`
//!    style pair.
//!
//! Each match becomes `<redacted>` with what identifies its kind kept (the
//! `/bot` path prefix, the `Bearer` word, the key's prefix, the pair's
//! name), so a reader still sees that a credential was there. It errs
//! toward redacting: a long opaque value after `secret:` goes even when it
//! was not a secret.

/// What replaces a secret.
pub const REDACTED: &str = "<redacted>";

/// Text with its secrets replaced, and how many were.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redacted {
    pub text: String,
    pub count: usize,
}

/// `text` with every recognised secret replaced by [`REDACTED`].
pub fn redact(text: &str) -> Redacted {
    let mut count = 0;
    let mut out = text.to_string();
    for pass in [bot_tokens, auth_schemes, prefixed_keys, named_values] {
        let (next, n) = pass(&out);
        out = next;
        count += n;
    }
    Redacted { text: out, count }
}

/// Characters a token is made of.
fn token_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-')
}

/// Characters a header credential or an opaque value is made of.
fn credential_char(c: u8) -> bool {
    token_char(c) || matches!(c, b'.' | b'~' | b'+' | b'/' | b'=' | b':')
}

fn word_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// The length of the run of bytes from `from` that `keep` accepts.
fn run(bytes: &[u8], from: usize, keep: fn(u8) -> bool) -> usize {
    bytes[from..].iter().take_while(|b| keep(**b)).count()
}

/// A Telegram bot token at `at`: `<6 or more digits>:<30 or more token
/// characters>`. Returns its length.
fn bot_token_at(bytes: &[u8], at: usize) -> Option<usize> {
    let digits = run(bytes, at, |c| c.is_ascii_digit());
    if digits < 6 || bytes.get(at + digits) != Some(&b':') {
        return None;
    }
    let secret = run(bytes, at + digits + 1, token_char);
    (secret >= 30).then_some(digits + 1 + secret)
}

/// Pass 1: `/bot<id>:<token>` in a URL, and a bare `<id>:<token>`.
fn bot_tokens(text: &str) -> (String, usize) {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut count = 0;
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        let after_bot = i >= 3 && bytes[i - 3..i].eq_ignore_ascii_case(b"bot");
        let starts_word = i == 0 || !word_char(bytes[i - 1]);
        if bytes[i].is_ascii_digit() && (after_bot || starts_word) {
            if let Some(len) = bot_token_at(bytes, i) {
                out.push_str(&text[copied..i]);
                out.push_str(REDACTED);
                count += 1;
                i += len;
                copied = i;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&text[copied..]);
    (out, count)
}

/// Pass 2: the credential after `Bearer ` or `Basic `.
fn auth_schemes(text: &str) -> (String, usize) {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut count = 0;
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        let scheme = [&b"bearer"[..], &b"basic"[..]].into_iter().find(|s| {
            bytes.len() >= i + s.len()
                && bytes[i..i + s.len()].eq_ignore_ascii_case(s)
                && (i == 0 || !word_char(bytes[i - 1]))
        });
        if let Some(scheme) = scheme {
            let mut value = i + scheme.len();
            let spaces = run(bytes, value, |c| c == b' ');
            value += spaces;
            let len = run(bytes, value, credential_char);
            if spaces >= 1 && len >= 8 {
                out.push_str(&text[copied..value]);
                out.push_str(REDACTED);
                count += 1;
                i = value + len;
                copied = i;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&text[copied..]);
    (out, count)
}

/// Key prefixes that are secrets by themselves, and how many token
/// characters must follow for the match to count.
const KEY_PREFIXES: [(&str, usize); 14] = [
    ("sk-", 16),
    ("sk_live_", 12),
    ("sk_test_", 12),
    ("rk_live_", 12),
    ("ghp_", 20),
    ("gho_", 20),
    ("ghs_", 20),
    ("ghu_", 20),
    ("github_pat_", 20),
    ("xoxb-", 16),
    ("xoxp-", 16),
    ("xoxa-", 16),
    ("xapp-", 16),
    ("AKIA", 16),
];

/// Pass 3: a key recognisable by its prefix. The prefix stays.
fn prefixed_keys(text: &str) -> (String, usize) {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut count = 0;
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        let boundary = i == 0 || !(word_char(bytes[i - 1]) || bytes[i - 1] == b'-');
        let hit = boundary
            .then(|| {
                KEY_PREFIXES.iter().find_map(|(prefix, min)| {
                    let p = prefix.as_bytes();
                    if bytes.len() < i + p.len() || &bytes[i..i + p.len()] != p {
                        return None;
                    }
                    let len = run(bytes, i + p.len(), token_char);
                    (len >= *min).then_some((p.len(), len))
                })
            })
            .flatten();
        if let Some((prefix, len)) = hit {
            out.push_str(&text[copied..i + prefix]);
            out.push_str(REDACTED);
            count += 1;
            i += prefix + len;
            copied = i;
            continue;
        }
        i += 1;
    }
    out.push_str(&text[copied..]);
    (out, count)
}

/// Names whose value is a secret wherever they appear as `name=value`,
/// `name: value` or `"name": "value"`.
const SECRET_NAMES: [&str; 16] = [
    "access_token",
    "refresh_token",
    "auth_token",
    "bot_token",
    "id_token",
    "token",
    "api_key",
    "api-key",
    "apikey",
    "x-api-key",
    "client_secret",
    "secret",
    "password",
    "passwd",
    "authorization",
    "cookie",
];

/// Pass 4: the value of a named secret. The name and separator stay.
fn named_values(text: &str) -> (String, usize) {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut count = 0;
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        let boundary = i == 0 || !(word_char(bytes[i - 1]) || bytes[i - 1] == b'-');
        let name = boundary
            .then(|| {
                SECRET_NAMES.iter().find(|n| {
                    let n = n.as_bytes();
                    bytes.len() > i + n.len() && bytes[i..i + n.len()].eq_ignore_ascii_case(n)
                })
            })
            .flatten();
        if let Some(name) = name {
            // name, an optional closing quote, `=` or `:`, optional spaces,
            // an optional opening quote, then the value.
            let mut at = i + name.len();
            if matches!(bytes.get(at), Some(b'"' | b'\'')) {
                at += 1;
            }
            if matches!(bytes.get(at), Some(b'=' | b':')) {
                at += 1;
                at += run(bytes, at, |c| c == b' ');
                let quote = bytes.get(at).copied().filter(|c| matches!(c, b'"' | b'\''));
                if quote.is_some() {
                    at += 1;
                }
                let len = match quote {
                    Some(q) => bytes[at..].iter().take_while(|c| **c != q).count(),
                    None => run(bytes, at, credential_char),
                };
                let value = &text[at..at + len];
                // `Authorization: Bearer <redacted>`: the scheme word is not
                // the secret, and pass 2 already took what followed it.
                let scheme =
                    value.eq_ignore_ascii_case("bearer") || value.eq_ignore_ascii_case("basic");
                if len >= 6 && !scheme && !value.contains(REDACTED) {
                    out.push_str(&text[copied..at]);
                    out.push_str(REDACTED);
                    count += 1;
                    i = at + len;
                    copied = i;
                    continue;
                }
            }
        }
        i += 1;
    }
    out.push_str(&text[copied..]);
    (out, count)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake token in the shape of a real one.
    const FAKE: &str = "7000000001:AAFakeFakeFakeFakeFakeFakeFakeFake_0";

    #[test]
    fn the_getupdates_error_line_loses_its_bot_token() {
        let line = format!(
            "2026-10-13T17:10:36.123456Z ERROR rustykrab_channels::telegram: Telegram getUpdates \
             network error: error sending request for url \
             (https://api.telegram.org/bot{FAKE}/getUpdates?timeout=30) consecutive_errors=1 \
             delay_secs=2"
        );
        let r = redact(&line);
        assert_eq!(r.count, 1);
        assert!(!r.text.contains("AAFake"), "{}", r.text);
        assert!(!r.text.contains("7000000001"), "{}", r.text);
        assert!(
            r.text
                .contains("https://api.telegram.org/bot<redacted>/getUpdates?timeout=30"),
            "{}",
            r.text
        );
        // Everything else on the line is kept.
        assert!(r.text.starts_with("2026-10-13T17:10:36.123456Z ERROR"));
        assert!(r.text.ends_with("consecutive_errors=1 delay_secs=2"));
    }

    #[test]
    fn a_bare_bot_token_goes_too() {
        let r = redact(&format!("TELEGRAM_BOT_TOKEN is {FAKE}."));
        assert_eq!(r.text, "TELEGRAM_BOT_TOKEN is <redacted>.");
        // A time, a port or a ratio is not a token.
        for plain in [
            "at 17:10:36 on port 127.0.0.1:65031",
            "run 123456:abc ended",
            "spent 1000000:1 tokens",
        ] {
            assert_eq!(redact(plain).text, plain);
        }
    }

    #[test]
    fn bearer_and_basic_credentials_are_redacted() {
        let r = redact("Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.e30.abc123_-xyz and more");
        assert_eq!(r.text, "Authorization: Bearer <redacted> and more");
        let r = redact("sent header authorization=Basic dXNlcjpwYXNzd29yZA==");
        assert_eq!(r.text, "sent header authorization=Basic <redacted>");
        assert_eq!(r.count, 1);
        // The word alone, or a short word after it, is prose.
        for plain in ["the bearer of news", "Bearer token missing", "basic setup"] {
            assert_eq!(redact(plain).text, plain, "{plain}");
        }
    }

    #[test]
    fn prefixed_keys_keep_only_their_prefix() {
        // Each fake key is put together at run time, so the source holds
        // no literal a secret scanner would take for a real one.
        let fake = |prefix: &str, body: &str| format!("{prefix}{body}");
        for (secret, kept) in [
            (
                fake("sk-", "ant-api03-AbCdEfGhIjKlMnOpQrStUvWx"),
                "sk-<redacted>",
            ),
            (fake("sk-", "proj-0123456789abcdefghij"), "sk-<redacted>"),
            (fake("sk_live_", "0123456789abcdef"), "sk_live_<redacted>"),
            (
                fake("ghp_", "0123456789abcdefghijABCDEFGHIJ012345"),
                "ghp_<redacted>",
            ),
            (
                fake("github_pat_", "11AAAAAAA0abcdefghijklmn_opqrstuv"),
                "github_pat_<redacted>",
            ),
            (
                fake("xoxb-", "1234567890-abcdefghijklmnop"),
                "xoxb-<redacted>",
            ),
            (fake("AKIA", "IOSFODNN7EXAMPLE"), "AKIA<redacted>"),
        ] {
            let r = redact(&format!("key {secret} used"));
            assert_eq!(r.text, format!("key {kept} used"), "{secret}");
            assert_eq!(r.count, 1, "{secret}");
        }
        // Words that merely start the same way are left alone.
        for plain in [
            "task-list",
            "risk-free-rate",
            "sk-1",
            "desk-0123456789abcdefghij",
        ] {
            assert_eq!(redact(plain).text, plain, "{plain}");
        }
    }

    #[test]
    fn named_values_are_redacted_and_their_names_kept() {
        for (line, want) in [
            (
                "GET /hook?token=abcdef123456&chat=7",
                "GET /hook?token=<redacted>&chat=7",
            ),
            (
                r#"body {"api_key": "live-0123456789", "user": "geoff"}"#,
                r#"body {"api_key": "<redacted>", "user": "geoff"}"#,
            ),
            ("password: hunter2hunter2", "password: <redacted>"),
            ("X-Api-Key: 0123456789abcdef", "X-Api-Key: <redacted>"),
            ("client_secret='s3cr3t-value'", "client_secret='<redacted>'"),
        ] {
            assert_eq!(redact(line).text, want, "{line}");
        }
        // A count of tokens, or a name with no value, is not a secret.
        for plain in [
            "3 iterations, 0 completion reminders, 1000 tokens, 1500 ms",
            "tokens=1000 wall_ms=1500",
            "the token: none",
            "token_budget=60000",
        ] {
            assert_eq!(redact(plain).text, plain, "{plain}");
        }
    }

    #[test]
    fn one_line_with_several_secrets_counts_each() {
        let key = format!("{}{}", "sk-", "ant-api03-AbCdEfGhIjKlMnOpQrStUvWx");
        let r = redact(&format!(
            "url (https://api.telegram.org/bot{FAKE}/sendMessage) Authorization: Bearer \
             abcdefgh12345678 key {key}"
        ));
        assert_eq!(r.count, 3, "{}", r.text);
        for leaked in ["AAFake", "abcdefgh12345678", "AbCdEfGh"] {
            assert!(!r.text.contains(leaked), "{}", r.text);
        }
    }

    #[test]
    fn text_without_secrets_is_unchanged_and_non_ascii_survives() {
        let line = "2026-10-13T17:10:36Z  WARN rustykrab_control: item café-☕ verification failed";
        let r = redact(line);
        assert_eq!(r.text, line);
        assert_eq!(r.count, 0);
        let r = redact(&format!("naïve ☕ bot{FAKE} ☕"));
        assert_eq!(r.text, "naïve ☕ bot<redacted> ☕");
    }
}
