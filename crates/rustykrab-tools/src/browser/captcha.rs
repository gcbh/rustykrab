//! Bounded observability for model-assisted CAPTCHA interactions.
//!
//! This does not contain a CAPTCHA bypass or token injector. The model uses
//! the ordinary visible browser controls, but explicitly marks an action as a
//! CAPTCHA attempt. That opt-in lets RustyKrab group interactions into one
//! challenge episode, enforce a small budget, and emit privacy-safe evidence
//! about progress, clearance, failure, and uncertainty.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::Mutex;
use uuid::Uuid;

const MAX_RECENT_ATTEMPTS: usize = 100;

#[derive(Debug, Clone)]
struct ActiveChallenge {
    id: Uuid,
    origin: String,
    providers: Vec<String>,
    started_at: Instant,
    attempts: u32,
    failure_recorded: bool,
}

#[derive(Debug, Default, Clone)]
struct Totals {
    challenges_detected: u64,
    attempts: u64,
    challenges_cleared: u64,
    challenges_failed: u64,
    actions_failed: u64,
    uncertain_attempts: u64,
    attempts_in_progress: u64,
    attempts_cleared: u64,
    attempts_budget_exhausted: u64,
    cleared_outside_tagged_confirmation: u64,
}

#[derive(Debug, Default)]
struct State {
    active: HashMap<String, ActiveChallenge>,
    totals: Totals,
    recent: VecDeque<Value>,
}

/// One authorized interaction within a detected challenge episode.
#[derive(Debug, Clone)]
pub(crate) struct AttemptTicket {
    key: String,
    challenge_id: Uuid,
    origin: String,
    providers: Vec<String>,
    attempt: u32,
    started_at: Instant,
    max_attempts: u32,
    timeout: Duration,
}

/// Beginning an attempt either produces a ticket or a structured budget
/// rejection that should be returned to the model without touching the page.
#[derive(Debug)]
pub(crate) enum AttemptStart {
    Ready(AttemptTicket),
    Rejected(Value),
}

/// Per-process challenge state. Durable experiment records are emitted by the
/// agent runner from each returned `captcha_attempt` object; this store exists
/// to enforce budgets and make live status useful between those writes.
#[derive(Debug, Clone, Default)]
pub(crate) struct CaptchaMonitor {
    inner: Arc<Mutex<State>>,
}

fn providers(captcha: &Value) -> Vec<String> {
    let mut values: Vec<String> = captcha["providers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|provider| {
            provider
                .chars()
                .filter(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
                })
                .take(48)
                .collect::<String>()
        })
        .filter(|provider| !provider.is_empty())
        .collect();
    values.sort();
    values.dedup();
    if values.is_empty() && captcha["detected"] == true {
        values.push("unknown".to_string());
    }
    values
}

fn detection_is_verified(captcha: &Value) -> bool {
    captcha["detected"].is_boolean() && captcha["status"].as_str() != Some("unverified")
}

fn detected(captcha: &Value) -> bool {
    captcha["detected"].as_bool() == Some(true)
}

fn challenge_value(
    challenge: &ActiveChallenge,
    enabled: bool,
    max_attempts: u32,
    timeout: Duration,
) -> Value {
    json!({
        "status": if challenge.failure_recorded { "budget_exhausted" } else { "detected" },
        "challenge_id": challenge.id,
        "origin": challenge.origin,
        "providers": challenge.providers,
        "attempts": challenge.attempts,
        "model_solver_enabled": enabled,
        "max_attempts": max_attempts,
        "timeout_ms": timeout.as_millis().min(u64::MAX as u128) as u64,
        "instruction": if enabled {
            "Use a screenshot, then mark only challenge-specific interactions with captchaAttempt=true. Stop on cleared, budget_exhausted, not_detected, or unknown. After action_failed, inspect the returned state and continue only with a materially corrected action while budget remains."
        } else {
            "Model-assisted CAPTCHA interaction is disabled for this browser profile."
        },
    })
}

impl CaptchaMonitor {
    /// Observe a page without counting it as a model attempt. A verified
    /// detection creates or refreshes an episode; a verified absence closes
    /// an episode that disappeared without an explicitly tagged model action.
    pub(crate) async fn observe(
        &self,
        key: &str,
        origin: &str,
        captcha: &Value,
        enabled: bool,
        max_attempts: u32,
        timeout: Duration,
    ) -> Value {
        let mut state = self.inner.lock().await;
        if !detection_is_verified(captcha) {
            return json!({
                "status": "unverified",
                "model_solver_enabled": enabled,
                "reason": "CAPTCHA detection did not complete",
            });
        }

        if !detected(captcha) {
            if state.active.remove(key).is_some() {
                state.totals.cleared_outside_tagged_confirmation = state
                    .totals
                    .cleared_outside_tagged_confirmation
                    .saturating_add(1);
            }
            return json!({
                "status": "not_detected",
                "model_solver_enabled": enabled,
            });
        }

        let observed_providers = providers(captcha);
        let replace = state
            .active
            .get(key)
            .is_some_and(|challenge| challenge.origin != origin);
        if replace {
            state.active.remove(key);
            state.totals.challenges_failed = state.totals.challenges_failed.saturating_add(1);
        }
        if !state.active.contains_key(key) {
            state.totals.challenges_detected = state.totals.challenges_detected.saturating_add(1);
            state.active.insert(
                key.to_string(),
                ActiveChallenge {
                    id: Uuid::new_v4(),
                    origin: origin.to_string(),
                    providers: observed_providers,
                    started_at: Instant::now(),
                    attempts: 0,
                    failure_recorded: false,
                },
            );
        } else if let Some(challenge) = state.active.get_mut(key) {
            challenge.providers = observed_providers;
        }
        challenge_value(
            state.active.get(key).expect("challenge was inserted"),
            enabled,
            max_attempts,
            timeout,
        )
    }

    /// Reserve one interaction from the episode budget.
    pub(crate) async fn begin_attempt(
        &self,
        key: &str,
        origin: &str,
        captcha: &Value,
        max_attempts: u32,
        timeout: Duration,
    ) -> AttemptStart {
        let mut state = self.inner.lock().await;
        if !detection_is_verified(captcha) {
            return AttemptStart::Rejected(json!({
                "result": "unknown",
                "outcome": "not_applied",
                "retry_safe": true,
                "reason": "CAPTCHA presence could not be verified before the interaction",
            }));
        }
        if !detected(captcha) {
            return AttemptStart::Rejected(json!({
                "result": "not_detected",
                "outcome": "not_applied",
                "retry_safe": true,
                "reason": "captchaAttempt=true requires a currently detected CAPTCHA",
            }));
        }

        let observed_providers = providers(captcha);
        let replace = state
            .active
            .get(key)
            .is_some_and(|challenge| challenge.origin != origin);
        if replace {
            state.active.remove(key);
            state.totals.challenges_failed = state.totals.challenges_failed.saturating_add(1);
        }
        if !state.active.contains_key(key) {
            state.totals.challenges_detected = state.totals.challenges_detected.saturating_add(1);
            state.active.insert(
                key.to_string(),
                ActiveChallenge {
                    id: Uuid::new_v4(),
                    origin: origin.to_string(),
                    providers: observed_providers.clone(),
                    started_at: Instant::now(),
                    attempts: 0,
                    failure_recorded: false,
                },
            );
        }

        let exhausted = state.active.get(key).and_then(|challenge| {
            if challenge.started_at.elapsed() >= timeout {
                Some("challenge timeout elapsed")
            } else if challenge.attempts >= max_attempts {
                Some("maximum challenge interactions reached")
            } else {
                None
            }
        });
        if let Some(reason) = exhausted {
            let (challenge_id, challenge_origin, challenge_providers, challenge_attempts, first) = {
                let challenge = state.active.get_mut(key).expect("checked above");
                let first = !challenge.failure_recorded;
                challenge.failure_recorded = true;
                (
                    challenge.id,
                    challenge.origin.clone(),
                    challenge.providers.clone(),
                    challenge.attempts,
                    first,
                )
            };
            if first {
                state.totals.challenges_failed = state.totals.challenges_failed.saturating_add(1);
            }
            return AttemptStart::Rejected(json!({
                "challenge_id": challenge_id,
                "origin": challenge_origin,
                "providers": challenge_providers,
                "attempt": challenge_attempts,
                "result": "budget_exhausted",
                "outcome": "not_applied",
                "retry_safe": false,
                "reason": reason,
                "max_attempts": max_attempts,
                "timeout_ms": timeout.as_millis().min(u64::MAX as u128) as u64,
            }));
        }

        let challenge = state.active.get_mut(key).expect("challenge was inserted");
        challenge.providers = observed_providers;
        challenge.attempts = challenge.attempts.saturating_add(1);
        let ticket = AttemptTicket {
            key: key.to_string(),
            challenge_id: challenge.id,
            origin: challenge.origin.clone(),
            providers: challenge.providers.clone(),
            attempt: challenge.attempts,
            started_at: challenge.started_at,
            max_attempts,
            timeout,
        };
        state.totals.attempts = state.totals.attempts.saturating_add(1);
        AttemptStart::Ready(ticket)
    }

    /// Classify one action from its explicit action outcome plus two CAPTCHA
    /// observations: the action's post-state snapshot and a delayed direct
    /// detector probe. Only two verified absences count as clearance.
    pub(crate) async fn finish_attempt(
        &self,
        ticket: AttemptTicket,
        action: &str,
        action_outcome: &str,
        post_state: &Value,
        confirmation: &Value,
    ) -> Value {
        let post_verified = detection_is_verified(post_state);
        let confirmation_verified = detection_is_verified(confirmation);
        let cleared_twice = post_verified
            && confirmation_verified
            && !detected(post_state)
            && !detected(confirmation);
        let still_detected = detected(post_state) || detected(confirmation);

        let mut result = match action_outcome {
            "not_applied" => "action_failed",
            "unknown" => "unknown",
            "applied" if cleared_twice => "cleared",
            "applied" if still_detected => "in_progress",
            "applied" => "unknown",
            _ => "unknown",
        };

        let mut state = self.inner.lock().await;
        let budget_exhausted = result == "in_progress"
            && (ticket.attempt >= ticket.max_attempts
                || ticket.started_at.elapsed() >= ticket.timeout);
        if budget_exhausted {
            result = "budget_exhausted";
        }

        match result {
            "cleared" => {
                state.active.remove(&ticket.key);
                state.totals.challenges_cleared = state.totals.challenges_cleared.saturating_add(1);
                state.totals.attempts_cleared = state.totals.attempts_cleared.saturating_add(1);
            }
            "budget_exhausted" => {
                let first = state.active.get_mut(&ticket.key).is_some_and(|challenge| {
                    let first = !challenge.failure_recorded;
                    challenge.failure_recorded = true;
                    first
                });
                if first {
                    state.totals.challenges_failed =
                        state.totals.challenges_failed.saturating_add(1);
                }
                state.totals.attempts_budget_exhausted =
                    state.totals.attempts_budget_exhausted.saturating_add(1);
            }
            "action_failed" => {
                state.totals.actions_failed = state.totals.actions_failed.saturating_add(1);
            }
            "unknown" => {
                state.totals.uncertain_attempts = state.totals.uncertain_attempts.saturating_add(1);
            }
            "in_progress" => {
                state.totals.attempts_in_progress =
                    state.totals.attempts_in_progress.saturating_add(1);
            }
            _ => {}
        }

        let value = json!({
            "challenge_id": ticket.challenge_id,
            "origin": ticket.origin,
            "providers": ticket.providers,
            "attempt": ticket.attempt,
            "action": action,
            "result": result,
            "action_outcome": action_outcome,
            "captcha_detected_after": still_detected,
            "clearance_confirmations": if cleared_twice { 2 } else { 0 },
            "elapsed_ms": ticket.started_at.elapsed().as_millis().min(u64::MAX as u128) as u64,
            "max_attempts": ticket.max_attempts,
            "timeout_ms": ticket.timeout.as_millis().min(u64::MAX as u128) as u64,
            "retry_safe": !matches!(result, "unknown" | "budget_exhausted"),
        });
        state.recent.push_back(value.clone());
        while state.recent.len() > MAX_RECENT_ATTEMPTS {
            state.recent.pop_front();
        }

        tracing::info!(
            challenge_id = %ticket.challenge_id,
            origin = %ticket.origin,
            providers = ?ticket.providers,
            attempt = ticket.attempt,
            action,
            action_outcome,
            result,
            elapsed_ms = ticket.started_at.elapsed().as_millis() as u64,
            clearance_confirmations = if cleared_twice { 2 } else { 0 },
            max_attempts = ticket.max_attempts,
            "model CAPTCHA interaction observed"
        );
        value
    }

    pub(crate) async fn status(
        &self,
        key: &str,
        enabled: bool,
        max_attempts: u32,
        timeout: Duration,
    ) -> Value {
        let state = self.inner.lock().await;
        let active = state
            .active
            .get(key)
            .map(|challenge| challenge_value(challenge, enabled, max_attempts, timeout));
        json!({
            "model_solver_enabled": enabled,
            "max_attempts": max_attempts,
            "timeout_ms": timeout.as_millis().min(u64::MAX as u128) as u64,
            "active": active,
            "totals": {
                "challenges_detected": state.totals.challenges_detected,
                "attempts": state.totals.attempts,
                "challenges_cleared": state.totals.challenges_cleared,
                "challenges_failed": state.totals.challenges_failed,
                "actions_failed": state.totals.actions_failed,
                "uncertain_attempts": state.totals.uncertain_attempts,
                "attempts_in_progress": state.totals.attempts_in_progress,
                "attempts_cleared": state.totals.attempts_cleared,
                "attempts_budget_exhausted": state.totals.attempts_budget_exhausted,
                "cleared_outside_tagged_confirmation": state.totals.cleared_outside_tagged_confirmation,
            },
            "recent_attempts": state.recent.iter().rev().take(10).cloned().collect::<Vec<_>>(),
            "durable_capture": "Set RUSTYKRAB_OUTCOME_CAPTURE=1 to persist per-model attempt records for offline evaluation.",
        })
    }
}

/// Keep only the URL origin. Query strings and paths can contain secrets or
/// personal data and are not needed to compare solve behavior by site.
pub(crate) fn safe_origin(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|parsed| {
            let host = parsed.host_str()?;
            let port = parsed
                .port()
                .map(|port| format!(":{port}"))
                .unwrap_or_default();
            Some(format!("{}://{}{}", parsed.scheme(), host, port))
        })
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn captcha(detected: bool) -> Value {
        json!({"detected":detected,"providers":if detected { json!(["recaptcha"]) } else { json!([]) }})
    }

    #[tokio::test]
    async fn a_challenge_keeps_one_id_until_two_observations_clear_it() {
        let monitor = CaptchaMonitor::default();
        let key = "conversation:profile:target";
        let timeout = Duration::from_secs(120);
        let first = monitor
            .observe(key, "https://example.com", &captcha(true), true, 3, timeout)
            .await;
        let ticket = match monitor
            .begin_attempt(key, "https://example.com", &captcha(true), 3, timeout)
            .await
        {
            AttemptStart::Ready(ticket) => ticket,
            AttemptStart::Rejected(value) => panic!("unexpected rejection: {value}"),
        };
        assert_eq!(first["challenge_id"], json!(ticket.challenge_id));

        let progress = monitor
            .finish_attempt(ticket, "click", "applied", &captcha(true), &captcha(true))
            .await;
        assert_eq!(progress["result"], "in_progress");

        let ticket = match monitor
            .begin_attempt(key, "https://example.com", &captcha(true), 3, timeout)
            .await
        {
            AttemptStart::Ready(ticket) => ticket,
            AttemptStart::Rejected(value) => panic!("unexpected rejection: {value}"),
        };
        let cleared = monitor
            .finish_attempt(ticket, "click", "applied", &captcha(false), &captcha(false))
            .await;
        assert_eq!(cleared["result"], "cleared");
        assert_eq!(cleared["clearance_confirmations"], 2);

        let status = monitor.status(key, true, 3, timeout).await;
        assert!(status["active"].is_null());
        assert_eq!(status["totals"]["attempts"], 2);
        assert_eq!(status["totals"]["challenges_cleared"], 1);
        assert_eq!(status["totals"]["attempts_in_progress"], 1);
        assert_eq!(status["totals"]["attempts_cleared"], 1);
    }

    #[tokio::test]
    async fn the_budget_stops_repeated_model_interactions() {
        let monitor = CaptchaMonitor::default();
        let key = "one";
        let timeout = Duration::from_secs(120);
        let ticket = match monitor
            .begin_attempt(key, "https://example.com", &captcha(true), 1, timeout)
            .await
        {
            AttemptStart::Ready(ticket) => ticket,
            AttemptStart::Rejected(value) => panic!("unexpected rejection: {value}"),
        };
        let stopped = monitor
            .finish_attempt(ticket, "click", "applied", &captcha(true), &captcha(true))
            .await;
        assert_eq!(stopped["result"], "budget_exhausted");
        let status = monitor.status(key, true, 1, timeout).await;
        assert_eq!(status["totals"]["attempts_budget_exhausted"], 1);

        let rejected = match monitor
            .begin_attempt(key, "https://example.com", &captcha(true), 1, timeout)
            .await
        {
            AttemptStart::Rejected(value) => value,
            AttemptStart::Ready(_) => panic!("a visible exhausted challenge must not reset"),
        };
        assert_eq!(rejected["result"], "budget_exhausted");
        assert_eq!(rejected["challenge_id"], stopped["challenge_id"]);
        let status = monitor.status(key, true, 1, timeout).await;
        assert_eq!(status["totals"]["challenges_failed"], 1);

        let fresh = monitor
            .begin_attempt(key, "https://example.com", &captcha(false), 1, timeout)
            .await;
        assert!(matches!(fresh, AttemptStart::Rejected(_)));
    }

    #[test]
    fn origins_drop_paths_queries_and_fragments() {
        assert_eq!(
            safe_origin("https://example.com:8443/login?token=secret#step"),
            "https://example.com:8443"
        );
        assert_eq!(safe_origin("not a url"), "unknown");
    }
}
