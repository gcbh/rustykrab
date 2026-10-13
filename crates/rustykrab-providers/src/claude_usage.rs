//! Parse only native, zero-inference `/usage` results. Never retain raw CLI text.
use std::collections::BTreeMap;

use chrono::{DateTime, Datelike, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClaudeQuotaWindow {
    pub label: String,
    pub used_percent: Option<f64>,
    pub exhausted: bool,
    pub resets_at: Option<i64>,
}

impl ClaudeQuotaWindow {
    pub(super) fn applies(&self, key: &str, model: Option<&str>) -> bool {
        match key.strip_prefix("seven_day_") {
            None => true,
            Some(family) => model.is_none_or(|m| m.to_ascii_lowercase().contains(family)),
        }
    }
}

pub(super) fn blocked_until(
    windows: &BTreeMap<String, ClaudeQuotaWindow>,
    model: Option<&str>,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    windows
        .iter()
        .filter(|(key, w)| w.exhausted && w.applies(key, model))
        .filter_map(|(_, w)| match w.resets_at {
            Some(at) => DateTime::from_timestamp(at, 0).filter(|at| *at > now),
            None => Some(now + chrono::TimeDelta::minutes(5)),
        })
        .max()
}

pub(super) fn parse_usage(
    value: &Value,
    now: DateTime<Utc>,
) -> Result<BTreeMap<String, ClaudeQuotaWindow>, String> {
    if value["type"] != "result"
        || value["local_command"] != "usage"
        || value["is_error"] != false
        || value["num_turns"] != 0
        || value["duration_api_ms"] != 0
    {
        return Err("Claude CLI did not return a native, zero-inference usage observation".into());
    }
    let text = value["result"]
        .as_str()
        .ok_or("Claude CLI usage observation was unreadable")?;
    let mut windows = BTreeMap::new();
    for line in text.lines().map(str::trim) {
        let (key, label, tail) = if let Some(tail) = line.strip_prefix("Current session: ") {
            ("five_hour".to_string(), "Current session".to_string(), tail)
        } else if let Some(tail) = line.strip_prefix("Current week (all models): ") {
            (
                "seven_day".to_string(),
                "Current week (all models)".to_string(),
                tail,
            )
        } else if let Some(tail) = line.strip_prefix("Current week (") {
            let (family, tail) = tail
                .split_once("): ")
                .ok_or("Claude CLI usage window format changed")?;
            if family.is_empty()
                || family.len() > 32
                || !family.chars().all(|c| c.is_ascii_alphabetic())
            {
                return Err("Claude CLI usage model window was unrecognized".into());
            }
            (
                format!("seven_day_{}", family.to_ascii_lowercase()),
                format!("Current week ({family})"),
                tail,
            )
        } else {
            continue;
        };
        let (percent, remainder) = tail
            .split_once("% used")
            .ok_or("Claude CLI usage percentage was missing")?;
        let used: f64 = percent
            .parse()
            .map_err(|_| "Claude CLI usage percentage was unreadable")?;
        if !used.is_finite() || !(0.0..=100.0).contains(&used) {
            return Err("Claude CLI usage percentage was out of range".into());
        }
        let resets_at = if remainder.trim().is_empty() {
            None
        } else {
            let reset = remainder
                .trim()
                .strip_prefix("· resets ")
                .ok_or("Claude CLI usage reset format changed")?;
            Some(parse_utc_reset(reset, now).ok_or("Claude CLI usage reset was unreadable")?)
        };
        if windows
            .insert(
                key,
                ClaudeQuotaWindow {
                    label,
                    used_percent: Some(used),
                    exhausted: used >= 100.0,
                    resets_at,
                },
            )
            .is_some()
        {
            return Err("Claude CLI usage observation repeated a window".into());
        }
    }
    // Refuse partial/changed output rather than inferring zero consumption.
    if !windows.contains_key("five_hour") || !windows.contains_key("seven_day") {
        return Err("Claude CLI did not report both session and weekly subscription usage".into());
    }
    Ok(windows)
}

fn parse_utc_reset(text: &str, now: DateTime<Utc>) -> Option<i64> {
    let text = text.strip_suffix(" (UTC)")?;
    let (date, time) = text.split_once(" at ")?;
    let (time, pm) = time
        .strip_suffix("am")
        .map(|t| (t, false))
        .or_else(|| time.strip_suffix("pm").map(|t| (t, true)))?;
    let (hour, minute) = time.split_once(':').unwrap_or((time, "0"));
    let hour: u32 = hour.parse().ok()?;
    let minute: u32 = minute.parse().ok()?;
    if !(1..=12).contains(&hour) {
        return None;
    }
    let hour = hour % 12 + if pm { 12 } else { 0 };
    // The CLI omits the year. Select the nearest bounded candidate, including
    // New Year's rollover; never interpret a malformed date as a year-long wait.
    (now.year() - 1..=now.year() + 1)
        .filter_map(|year| {
            NaiveDate::parse_from_str(&format!("{year} {date}"), "%Y %b %d")
                .ok()
                .and_then(|d| d.and_hms_opt(hour, minute, 0))
                .map(|at| at.and_utc())
        })
        .filter(|at| (*at - now).num_seconds().abs() <= 32 * 24 * 60 * 60)
        .min_by_key(|at| (*at - now).num_seconds().abs())
        .map(|at| at.timestamp())
}

pub(super) fn rate_limit_event(value: &Value) -> Option<(String, ClaudeQuotaWindow)> {
    if value["type"] != "rate_limit_event" {
        return None;
    }
    let info = &value["rate_limit_info"];
    let exhausted = match info["status"].as_str()? {
        "rejected" => true,
        "allowed" | "allowed_warning" => false,
        _ => return None,
    };
    let key = info["rateLimitType"].as_str()?;
    if ![
        "five_hour",
        "seven_day",
        "seven_day_opus",
        "seven_day_sonnet",
        "seven_day_fable",
    ]
    .contains(&key)
    {
        return None;
    }
    let used_percent = info["utilization"]
        .as_f64()
        .filter(|v| v.is_finite() && (0.0..=1.0).contains(v))
        .map(|v| v * 100.0);
    let resets_at = info["resetsAt"].as_i64().filter(|at| {
        DateTime::<Utc>::from_timestamp(*at, 0)
            .is_some_and(|reset| (reset - Utc::now()).num_seconds().abs() <= 32 * 24 * 60 * 60)
    });
    Some((
        key.into(),
        ClaudeQuotaWindow {
            label: key.replace('_', " "),
            used_percent,
            exhausted,
            resets_at,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn now() -> DateTime<Utc> {
        "2026-10-13T17:52:00Z".parse().unwrap()
    }
    fn envelope(text: &str) -> Value {
        json!({"type":"result","local_command":"usage","is_error":false,"num_turns":0,"duration_api_ms":0,"result":text})
    }
    #[test]
    fn live_weekly_exhaustion_blocks_until_wednesday_and_keeps_other_windows() {
        let v=envelope("Current session: 15% used · resets Oct 13 at 6:29pm (UTC)\nCurrent week (all models): 100% used · resets Oct 14 at 10:59pm (UTC)\nCurrent week (Fable): 0% used · resets Oct 14 at 11pm (UTC)");
        let windows = parse_usage(&v, now()).unwrap();
        assert_eq!(windows["five_hour"].used_percent, Some(15.0));
        assert_eq!(
            blocked_until(&windows, Some("opus"), now()),
            Some("2026-10-14T22:59:00Z".parse().unwrap())
        );
        assert!(blocked_until(&windows, None, "2026-10-14T23:01:00Z".parse().unwrap()).is_none());
        let serialized = serde_json::to_string(&windows).unwrap();
        assert!(!serialized.contains("local sessions"));
    }
    #[test]
    fn available_window_without_reset_is_not_a_zero_capacity_guess() {
        let windows=parse_usage(&envelope("Current session: 0% used\nCurrent week (all models): 0% used · resets Oct 17 at 6:59pm (UTC)"),now()).unwrap();
        assert_eq!(windows["five_hour"].resets_at, None);
        assert_eq!(blocked_until(&windows, None, now()), None);
    }
    #[test]
    fn rejects_generated_prose_partial_output_invalid_percent_and_timezone() {
        let good="Current session: 15% used\nCurrent week (all models): 100% used · resets Oct 14 at 10:59pm (UTC)";
        for field in ["local_command", "num_turns", "duration_api_ms"] {
            let mut value = envelope(good);
            value[field] = json!(1);
            assert!(parse_usage(&value, now()).is_err());
        }
        for text in [
            good.replace("100%", "NaN%"),
            good.replace("100%", "101%"),
            good.replace("UTC", "America/Los_Angeles"),
            good.replace("Oct 14", "Apr 14"),
            "Current session: 1% used".into(),
        ] {
            assert!(parse_usage(&envelope(&text), now()).is_err());
        }
    }
    #[test]
    fn model_specific_windows_do_not_disable_unaffected_models() {
        let windows=parse_usage(&envelope("Current session: 0% used\nCurrent week (all models): 10% used\nCurrent week (Opus): 100% used · resets Oct 14 at 11pm (UTC)"),now()).unwrap();
        assert!(blocked_until(&windows, Some("claude-opus-4-6"), now()).is_some());
        assert!(blocked_until(&windows, Some("sonnet"), now()).is_none());
        assert!(blocked_until(&windows, None, now()).is_some());
    }
    #[test]
    fn new_year_reset_resolves_to_the_next_year() {
        let now = "2026-12-31T23:00:00Z".parse().unwrap();
        let windows=parse_usage(&envelope("Current session: 100% used · resets Jan 1 at 2am (UTC)\nCurrent week (all models): 1% used"),now).unwrap();
        assert_eq!(
            blocked_until(&windows, None, now),
            Some("2027-01-01T02:00:00Z".parse().unwrap())
        );
    }
    #[test]
    fn native_limit_notifications_distinguish_warnings_from_rejections() {
        let mut value = json!({"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","rateLimitType":"seven_day","utilization":0.95,"resetsAt":1792018740,"private":"discard"}});
        let (key, window) = rate_limit_event(&value).unwrap();
        assert!(!window.exhausted);
        assert_eq!(window.used_percent, Some(95.0));
        assert_eq!(key, "seven_day");
        value["rate_limit_info"]["status"] = json!("rejected");
        assert!(rate_limit_event(&value).unwrap().1.exhausted);
        value["type"] = json!("assistant");
        assert!(rate_limit_event(&value).is_none());
    }
}
