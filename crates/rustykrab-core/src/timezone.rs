//! The wall clock the operator actually reads.
//!
//! Timestamps are stored and compared in UTC everywhere — that part is not
//! negotiable and nothing here changes it. What this module supplies is the
//! *lens*: the zone a human-entered time is written in, so `"0 9 * * *"`
//! means nine in the morning where the operator lives rather than nine in
//! Greenwich.
//!
//! Resolution order, first hit wins:
//!
//! 1. `RUSTYKRAB_TIMEZONE` — an IANA name such as `America/Los_Angeles`.
//! 2. The host's configured zone, via `iana-time-zone`.
//! 3. `UTC`, so a machine with no discoverable zone still starts.
//!
//! Prefer IANA names over fixed offsets. `America/Los_Angeles` tracks the
//! PST/PDT transition on its own; `UTC-8` silently drifts an hour every
//! spring.

use std::sync::OnceLock;

pub use chrono_tz::Tz;

use crate::Error;

/// Environment variable holding the operator's IANA zone name.
pub const TIMEZONE_ENV: &str = "RUSTYKRAB_TIMEZONE";

static CONFIGURED: OnceLock<Tz> = OnceLock::new();

/// The zone human-entered schedules are interpreted in.
///
/// Resolved once per process and cached: the daemon runs for weeks, and a
/// zone that changed underneath a running scheduler would move every job's
/// next fire without anything in the log explaining why.
pub fn configured() -> Tz {
    *CONFIGURED.get_or_init(resolve)
}

fn resolve() -> Tz {
    if let Ok(name) = std::env::var(TIMEZONE_ENV) {
        let name = name.trim();
        if !name.is_empty() {
            match parse(name) {
                Ok(tz) => return tz,
                Err(_) => {
                    tracing_warn(&format!(
                        "{TIMEZONE_ENV}='{name}' is not a known IANA zone; \
                         falling back to the host zone"
                    ));
                }
            }
        }
    }

    if let Ok(name) = iana_time_zone::get_timezone() {
        if let Ok(tz) = parse(&name) {
            return tz;
        }
    }

    Tz::UTC
}

/// Parse an IANA zone name, rejecting anything `chrono-tz` does not know.
pub fn parse(name: &str) -> Result<Tz, Error> {
    name.trim().parse::<Tz>().map_err(|_| {
        Error::Config(format!(
            "unknown timezone '{name}'. Use an IANA name such as \
             'America/Los_Angeles', 'Europe/Berlin', or 'UTC'"
        ))
    })
}

/// `tracing` is not a dependency of this crate; the one diagnostic path here
/// does not justify adding it.
fn tracing_warn(message: &str) {
    eprintln!("WARN rustykrab_core::timezone: {message}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    #[test]
    fn parses_iana_names() {
        assert_eq!(
            parse("America/Los_Angeles").unwrap(),
            Tz::America__Los_Angeles
        );
        assert_eq!(parse("  UTC  ").unwrap(), Tz::UTC);
    }

    #[test]
    fn rejects_junk_with_an_actionable_message() {
        // "PST" and "UTC-8" are the two things an operator reaches for first,
        // and both are wrong: the former is ambiguous, the latter freezes the
        // offset across the DST boundary. The error has to name the fix.
        let err = parse("UTC-8").expect_err("fixed offsets are not IANA names");
        assert!(
            err.to_string().contains("America/Los_Angeles"),
            "error should show a valid example: {err}"
        );
    }

    #[test]
    fn los_angeles_tracks_the_dst_transition() {
        // The whole reason a named zone beats a stored offset: the same
        // 09:00 wall-clock time is a different UTC instant in July than in
        // January, and only the zone database knows when it flips.
        let tz = Tz::America__Los_Angeles;
        let summer = tz.with_ymd_and_hms(2026, 7, 1, 9, 0, 0).unwrap();
        let winter = tz.with_ymd_and_hms(2026, 1, 1, 9, 0, 0).unwrap();
        assert_eq!(
            summer.with_timezone(&Utc).format("%H:%M").to_string(),
            "16:00"
        );
        assert_eq!(
            winter.with_timezone(&Utc).format("%H:%M").to_string(),
            "17:00"
        );
    }
}
