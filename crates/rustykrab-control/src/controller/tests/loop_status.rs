//! `loop_status` after ticks that fail and ticks that complete: what
//! `GET /api/version` reads to tell a failing loop from a stuck one.

use super::super::tick::failure_class;
use super::*;
use crate::handle::{LockState, LoopStatus};

impl Harness {
    /// Make the next tick fail with `err`, then tick and return its error.
    async fn failing_tick(&self, err: Error) -> Error {
        self.ctl.state().fail_next_tick = Some(err);
        self.clock.advance(TimeDelta::seconds(1));
        ControlHandle::tick(&self.ctl)
            .await
            .expect_err("the injected fault fails the tick")
    }

    fn loop_status(&self) -> LoopStatus {
        ControlHandle::loop_status(&self.ctl).expect("the controller runs a loop")
    }
}

#[tokio::test]
async fn a_controller_before_its_first_tick_reports_no_failures() {
    let h = Harness::new(&["pinch"]);
    let status = h.loop_status();
    assert_eq!(status.last_tick, None);
    assert_eq!(status.last_failed_tick, None);
    assert_eq!(status.last_failure_class, None);
    assert_eq!(status.consecutive_failed_ticks, 0);
}

#[tokio::test]
async fn a_failing_tick_is_recorded_with_its_time_and_class() {
    let h = Harness::new(&["pinch"]);
    let err = h
        .failing_tick(Error::Storage("database is locked".into()))
        .await;
    assert!(matches!(err, Error::Storage(_)), "{err}");

    let status = h.loop_status();
    assert_eq!(status.last_failed_tick, Some(h.clock.now()));
    assert_eq!(status.last_failure_class.as_deref(), Some("storage"));
    assert_eq!(status.consecutive_failed_ticks, 1);
    assert_eq!(
        status.last_tick, None,
        "a failed tick is not a completed one"
    );
}

#[tokio::test]
async fn consecutive_failures_count_up_and_keep_the_latest_class() {
    let h = Harness::new(&["pinch"]);
    h.failing_tick(Error::Storage("disk I/O error".into()))
        .await;
    h.failing_tick(Error::Storage("disk I/O error".into()))
        .await;
    h.failing_tick(Error::Internal("lost the plot".into()))
        .await;

    let status = h.loop_status();
    assert_eq!(status.consecutive_failed_ticks, 3);
    assert_eq!(status.last_failed_tick, Some(h.clock.now()));
    assert_eq!(status.last_failure_class.as_deref(), Some("internal"));
}

#[tokio::test]
async fn a_completed_tick_resets_the_count_and_keeps_the_last_failure() {
    let h = Harness::new(&["pinch"]);
    h.failing_tick(Error::Storage("disk I/O error".into()))
        .await;
    h.failing_tick(Error::Storage("disk I/O error".into()))
        .await;
    let failed_at = h.clock.now();

    h.tick().await;
    let status = h.loop_status();
    assert_eq!(status.consecutive_failed_ticks, 0);
    assert_eq!(status.last_tick, Some(h.clock.now()));
    assert!(status.last_tick > Some(failed_at));
    assert_eq!(status.last_failed_tick, Some(failed_at));
    assert_eq!(status.last_failure_class.as_deref(), Some("storage"));

    h.failing_tick(Error::Config("bad".into())).await;
    assert_eq!(h.loop_status().consecutive_failed_ticks, 1);
}

#[tokio::test]
async fn tick_refuses_while_the_loop_waits_on_the_lock_and_ticks_otherwise() {
    let h = Harness::new(&["pinch"]);

    h.ctl.state().lock = Some(LockState::Waiting);
    let err = ControlHandle::tick(&h.ctl)
        .await
        .expect_err("a waiting loop does not tick");
    assert!(matches!(err, Error::LockWaiting(_)), "{err}");
    assert!(err.to_string().contains("controller.lock"), "{err}");
    let status = h.loop_status();
    assert_eq!(status.last_tick, None, "the refusal ran no pass");
    assert_eq!(
        status.consecutive_failed_ticks, 0,
        "a refusal is not a failed tick"
    );

    h.ctl.state().lock = Some(LockState::Held);
    ControlHandle::tick(&h.ctl)
        .await
        .expect("a loop holding the lock ticks");
    assert_eq!(h.loop_status().last_tick, Some(h.clock.now()));

    h.clock.advance(TimeDelta::seconds(1));
    h.ctl.state().lock = None;
    ControlHandle::tick(&h.ctl)
        .await
        .expect("a controller no loop drives ticks");
    assert_eq!(h.loop_status().last_tick, Some(h.clock.now()));
}

#[test]
fn failure_classes_name_the_error_variant() {
    assert_eq!(failure_class(&Error::Storage(String::new())), "storage");
    assert_eq!(failure_class(&Error::NotFound(String::new())), "not_found");
    assert_eq!(
        failure_class(&Error::ModelRateLimit(String::new())),
        "model_rate_limit"
    );
    assert_eq!(failure_class(&Error::ContentPolicy), "content_policy");
}
