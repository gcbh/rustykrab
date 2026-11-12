//! The controller stays observable, keeps live runs' leases and honours a
//! cancel while a tick holds `loop_lock` for a long time (a machine
//! swapping hard slowed every store round-trip of a tick 40-140x).

use std::pin::pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use chrono::TimeDelta;
use rustykrab_core::work::{ResultReport, Status, WorkerKind};
use rustykrab_core::Error;

use super::*;
use crate::controller::tests::{draft, no_rungs, Harness};
use crate::controller::ControllerConfig;
use crate::handle::{ControlHandle, LoopStatus};
use crate::worker::{Brief, WorkerCapabilities};

/// A worker whose runs never return on their own but show activity when
/// the test bumps `pulse`, and which records the runs it was told to stop.
#[derive(Default)]
struct Pulsing {
    pulse: AtomicU64,
    runs: Mutex<Vec<String>>,
    stopped: Mutex<Vec<String>>,
}

#[async_trait]
impl Worker for Pulsing {
    fn name(&self) -> &str {
        "pulsing"
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::Codex
    }
    fn capabilities(&self) -> WorkerCapabilities {
        WorkerCapabilities {
            models: vec!["native-model".into()],
            ..Default::default()
        }
    }
    fn activity(&self, run: &str) -> Option<u64> {
        self.runs
            .lock()
            .unwrap()
            .iter()
            .any(|r| r == run)
            .then(|| self.pulse.load(Ordering::SeqCst))
    }
    fn stop(&self, run: &str) {
        self.stopped.lock().unwrap().push(run.to_string());
    }
    async fn run(&self, brief: Brief) -> Result<ResultReport, Error> {
        self.runs
            .lock()
            .unwrap()
            .push(brief.run.clone().unwrap_or_default());
        std::future::pending().await
    }
}

/// One item leased to a [`Pulsing`] worker whose run has started. A one
/// second lease TTL makes the heartbeat task look every 250 ms.
async fn running() -> (Harness, Arc<Pulsing>, String) {
    let worker = Arc::new(Pulsing::default());
    let h = Harness::with_fleet(
        ControllerConfig {
            lease_ttl_seconds: 1,
            ..Default::default()
        },
        vec![worker.clone()],
    );
    let mut d = draft("job", "inspect repository");
    d.budget = Some(no_rungs());
    let id = h.file_one(d).await;
    assert_eq!(h.tick().await.leased, vec![id.clone()]);
    wait_until("the run starts", || !worker.runs.lock().unwrap().is_empty()).await;
    (h, worker, id)
}

async fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(std::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn status(h: &Harness) -> LoopStatus {
    ControlHandle::loop_status(&h.ctl).expect("the controller runs a loop")
}

#[tokio::test]
async fn a_tick_waiting_in_a_phase_reports_its_start_and_phase() {
    let h = Harness::new(&["pinch"]);
    assert_eq!(status(&h).tick_phase, None);
    assert_eq!(status(&h).last_tick_ms, None);

    let held = h.ctl.loop_lock.lock().await;
    h.clock.advance(TimeDelta::seconds(1));
    let started = h.clock.now();
    let mut tick = pin!(ControlHandle::tick(&h.ctl));
    tokio::select! {
        _ = &mut tick => panic!("the tick cannot pass loop_lock"),
        _ = tokio::time::sleep(Duration::from_millis(100)) => {}
    }
    let during = status(&h);
    assert_eq!(during.tick_phase.as_deref(), Some("loop_lock"));
    assert_eq!(during.tick_started, Some(started));
    assert_eq!(during.last_tick, None, "nothing has completed yet");
    // At least 100 ms in loop_lock, counted from when the phase began.
    tokio::time::sleep(Duration::from_millis(100)).await;

    drop(held);
    tick.await
        .expect("the tick completes once the lock is free");
    let after = status(&h);
    assert_eq!(after.tick_phase, None);
    assert_eq!(after.tick_started, None);
    assert!(after.last_tick_ms.is_some_and(|ms| ms >= 100), "{after:?}");
    assert_eq!(
        after.last_tick_slowest_phase.as_deref(),
        Some("loop_lock"),
        "{after:?}"
    );
    assert!(after.last_tick_slowest_phase_ms.is_some_and(|ms| ms >= 100));
}

#[tokio::test]
async fn a_failed_or_dropped_tick_still_ends_its_timing() {
    let h = Harness::new(&["pinch"]);
    h.ctl.state().fail_next_tick = Some(Error::Storage("disk I/O error".into()));
    ControlHandle::tick(&h.ctl)
        .await
        .expect_err("the injected fault fails the tick");
    let failed = status(&h);
    assert_eq!(failed.tick_phase, None);
    assert!(failed.last_tick_ms.is_some());

    let held = h.ctl.loop_lock.lock().await;
    {
        let mut tick = pin!(ControlHandle::tick(&h.ctl));
        tokio::select! {
            _ = &mut tick => panic!("the tick cannot pass loop_lock"),
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
        assert_eq!(status(&h).tick_phase.as_deref(), Some("loop_lock"));
    }
    assert_eq!(status(&h).tick_phase, None, "a dropped tick is not shown");
    drop(held);
}

#[tokio::test]
async fn activity_renews_the_lease_while_a_tick_holds_loop_lock() {
    let (h, worker, id) = running().await;
    let lease = || async { h.store().work_lease_get(&id).await.unwrap().unwrap() };
    let first = lease().await.heartbeat_at;

    let held = h.ctl.loop_lock.lock().await;
    worker.pulse.store(1, Ordering::SeqCst);
    let mut renewed = first;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while renewed <= first {
        assert!(std::time::Instant::now() < deadline, "no heartbeat");
        tokio::time::sleep(Duration::from_millis(50)).await;
        renewed = lease().await.heartbeat_at;
    }
    // Silence writes nothing; the next sign of life renews it again.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(lease().await.heartbeat_at, renewed, "no activity, no beat");
    worker.pulse.store(2, Ordering::SeqCst);
    let mut again = renewed;
    while again <= renewed {
        assert!(std::time::Instant::now() < deadline, "no second heartbeat");
        tokio::time::sleep(Duration::from_millis(50)).await;
        again = lease().await.heartbeat_at;
    }
    drop(held);

    // The tick sees the same observation, not a second change: the run is
    // alive and the TTL counts from the heartbeat task's observation.
    h.tick().await;
    assert_eq!(h.status(&id).await, Status::Running);
}

#[tokio::test]
async fn the_heartbeat_task_ends_with_its_run() {
    let (h, worker, id) = running().await;
    let guard = {
        let state = h.ctl.state();
        let run = &state.runs[&id];
        assert!(!run.handle.is_finished());
        run.handle.abort_handle()
    };
    // Stop the run as a cancel or stall does: the run leaves the map and
    // its heartbeat guard aborts the task, so new activity writes nothing.
    let run = h.ctl.state().runs.remove(&id).unwrap();
    guard.abort();
    drop(run);
    let before = h.store().work_lease_get(&id).await.unwrap().unwrap();
    worker.pulse.store(7, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(600)).await;
    let after = h.store().work_lease_get(&id).await.unwrap().unwrap();
    assert_eq!(after.heartbeat_at, before.heartbeat_at);
}

#[tokio::test]
async fn cancel_stops_the_run_without_waiting_for_the_tick() {
    let (h, worker, id) = running().await;
    let run_id = worker.runs.lock().unwrap()[0].clone();

    let held = h.ctl.loop_lock.lock().await;
    let mut cancel = pin!(ControlHandle::cancel(
        &h.ctl,
        &id,
        Some("not needed".into()),
        "user:master"
    ));
    tokio::select! {
        _ = &mut cancel => panic!("the cancel records under loop_lock"),
        _ = tokio::time::sleep(Duration::from_millis(100)) => {}
    }
    assert_eq!(*worker.stopped.lock().unwrap(), vec![run_id]);
    assert!(h.ctl.running().is_empty(), "the run is stopped already");

    // A tick that holds the lock meanwhile (its lease lapsing included)
    // neither judges the stopped run nor leases the item again.
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    h.clock.advance(TimeDelta::seconds(5));
    h.ctl
        .tick_locked(Ok(None), Ok(Default::default()))
        .await
        .expect("tick");
    assert_eq!(h.status(&id).await, Status::Running);
    assert!(h.ctl.running().is_empty());
    assert_eq!(worker.runs.lock().unwrap().len(), 1, "never leased again");
    assert!(crate::ladder::last_error(&h.events(&id).await).is_none());

    drop(held);
    let cancelled = cancel.await.expect("cancel");
    assert_eq!(cancelled, vec![id.clone()]);
    assert!(matches!(h.status(&id).await, Status::Cancelled(_)));
    assert!(
        crate::ladder::last_error(&h.events(&id).await).is_none(),
        "a cancel is not a failed attempt"
    );
    assert!(h.ctl.state().cancelling.is_empty());
    let spend = h.store().work_spend_totals().await.unwrap();
    assert!(
        spend.contains_key(&id),
        "the stopped run's spend is recorded"
    );
    h.tick().await;
    assert_eq!(worker.runs.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_cancel_dropped_while_waiting_leaves_nothing_marked() {
    let (h, worker, id) = running().await;
    let held = h.ctl.loop_lock.lock().await;
    {
        let mut cancel = pin!(ControlHandle::cancel(&h.ctl, &id, None, "user:master"));
        tokio::select! {
            _ = &mut cancel => panic!("the cancel records under loop_lock"),
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
        assert!(h.ctl.state().cancelling.contains(&id));
    }
    assert!(
        h.ctl.state().cancelling.is_empty(),
        "the dropped cancel's marks are lifted"
    );
    assert_eq!(worker.stopped.lock().unwrap().len(), 1);
    drop(held);
    // Not recorded: the item is still active, its stopped run gone, and its
    // lease returns it to ready when it lapses, with no failure counted.
    assert_eq!(h.status(&id).await, Status::Running);
}

/// A worker whose stop is not immediate (a peer) may still hand in its
/// result after the cancel stopped its run: the result is refused and the
/// tick that holds the lock meanwhile does not judge it, so the cancel wins.
#[tokio::test]
async fn a_result_handed_in_after_the_cancel_stopped_the_run_is_refused() {
    use rustykrab_tools::work_backend::{Provenance, WorkBackend};
    let (h, _worker, id) = running().await;
    let held = h.ctl.loop_lock.lock().await;
    let mut cancel = pin!(ControlHandle::cancel(&h.ctl, &id, None, "user:master"));
    tokio::select! {
        _ = &mut cancel => panic!("the cancel records under loop_lock"),
        _ = tokio::time::sleep(Duration::from_millis(50)) => {}
    }
    let late = WorkBackend::report(
        &h.ctl,
        id.clone(),
        crate::controller::tests::done("inspect repository"),
        Provenance {
            conversation_id: None,
            filed_by_item: Some(id.clone()),
            actor: "worker:pulsing".into(),
        },
    )
    .await;
    assert!(
        matches!(late, Err(Error::Auth(_))),
        "a stopped run's result is refused like a lost lease: {late:?}"
    );
    h.ctl
        .tick_locked(Ok(None), Ok(Default::default()))
        .await
        .expect("tick");
    assert_eq!(h.status(&id).await, Status::Running);
    drop(held);
    assert_eq!(cancel.await.expect("cancel"), vec![id.clone()]);
    assert!(matches!(h.status(&id).await, Status::Cancelled(_)));
}

/// A second tick beside the timed one (a manual `work tick` while the loop
/// ticks) neither replaces nor ends the first tick's record, even when it
/// is dropped.
#[tokio::test]
async fn a_concurrent_tick_does_not_touch_the_timed_ticks_record() {
    let h = Harness::new(&["pinch"]);
    let held = h.ctl.loop_lock.lock().await;
    h.clock.advance(TimeDelta::seconds(1));
    let started = h.clock.now();
    let mut first = pin!(ControlHandle::tick(&h.ctl));
    tokio::select! {
        _ = &mut first => panic!("the tick cannot pass loop_lock"),
        _ = tokio::time::sleep(Duration::from_millis(30)) => {}
    }
    h.clock.advance(TimeDelta::seconds(1));
    {
        let mut second = pin!(ControlHandle::tick(&h.ctl));
        tokio::select! {
            _ = &mut second => panic!("the tick cannot pass loop_lock"),
            _ = tokio::time::sleep(Duration::from_millis(30)) => {}
        }
        let during = status(&h);
        assert_eq!(during.tick_started, Some(started));
        assert_eq!(during.tick_phase.as_deref(), Some("loop_lock"));
    }
    let after_drop = status(&h);
    assert_eq!(after_drop.tick_started, Some(started), "{after_drop:?}");
    assert_eq!(after_drop.tick_phase.as_deref(), Some("loop_lock"));
    assert_eq!(after_drop.last_tick_ms, None);
    drop(held);
    first.await.expect("tick");
    let done = status(&h);
    assert_eq!(done.tick_phase, None);
    assert!(done.last_tick_ms.is_some());
}

/// The loop's tick queues untimed behind a timed manual tick that is then
/// dropped (its client went away): once it holds `loop_lock`, the loop's
/// tick claims the free record, so its run is timed and shown.
#[tokio::test]
async fn a_tick_queued_behind_a_dropped_timed_tick_is_timed() {
    let h = Harness::new(&["pinch"]);
    let held = h.ctl.loop_lock.lock().await;
    let mut queued = {
        let mut manual = Box::pin(ControlHandle::tick(&h.ctl));
        tokio::select! {
            _ = &mut manual => panic!("the tick cannot pass loop_lock"),
            _ = tokio::time::sleep(Duration::from_millis(30)) => {}
        }
        let manual_started = status(&h).tick_started;
        assert!(manual_started.is_some());
        h.clock.advance(TimeDelta::seconds(1));
        let mut queued = Box::pin(ControlHandle::tick(&h.ctl));
        tokio::select! {
            _ = &mut queued => panic!("the tick cannot pass loop_lock"),
            _ = tokio::time::sleep(Duration::from_millis(30)) => {}
        }
        assert_eq!(status(&h).tick_started, manual_started, "untimed meanwhile");
        queued
    };
    // The manual tick was dropped: its record ended.
    assert_eq!(status(&h).tick_phase, None);
    h.ctl.state().last_timing = None;
    let claimed_at = h.clock.now();
    // Free the lock: the queued tick takes it and claims the record, so
    // its pass ends with its own timing.
    drop(held);
    (&mut queued).await.expect("tick");
    let done = status(&h);
    assert!(
        done.last_tick_ms.is_some(),
        "the queued tick was timed once the record was free: {done:?}"
    );
    assert_eq!(done.tick_phase, None);
    assert!(h.ctl.state().last_tick.is_some_and(|t| t >= claimed_at));
}

/// Two cancels of the same item, the second arriving while the first waits
/// for the loop (a client that timed out and asked again; its first
/// request still completes on its own task): both answer with the item
/// the cancel closed. Before, the second answered `"cancelled": []`
/// although the item ended cancelled.
#[tokio::test]
async fn a_second_cancel_while_the_first_is_in_flight_names_what_was_cancelled() {
    let (h, _worker, id) = running().await;
    let held = h.ctl.loop_lock.lock().await;
    let mut first = pin!(ControlHandle::cancel(&h.ctl, &id, None, "user:master"));
    let mut second = pin!(ControlHandle::cancel(&h.ctl, &id, None, "user:master"));
    for cancel in [&mut first, &mut second] {
        tokio::select! {
            _ = cancel => panic!("the cancel records under loop_lock"),
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }
    drop(held);
    let (first, second) = tokio::join!(first, second);
    let (first, second) = (first.expect("first cancel"), second.expect("second cancel"));
    assert_eq!(first, vec![id.clone()]);
    assert_eq!(second, vec![id.clone()]);
    assert!(matches!(h.status(&id).await, Status::Cancelled(_)));
    // A later request for it still says what was cancelled.
    assert_eq!(
        ControlHandle::cancel(&h.ctl, &id, None, "user:master")
            .await
            .unwrap(),
        vec![id.clone()]
    );
}

/// What one cancel request closed answers a repeated request for the same
/// item, and never one for another item.
#[tokio::test]
async fn a_repeated_cancel_names_only_items_under_the_one_asked_for() {
    let h = Harness::new(&["pinch"]);
    let parent = h.file_one(draft("p", "Parent")).await;
    let cancelled = ControlHandle::cancel(&h.ctl, &parent, None, "user:master")
        .await
        .unwrap();
    assert_eq!(cancelled, vec![parent.clone()]);
    let other = h.file_one(draft("o", "Other")).await;
    h.ctl
        .cancel_locked(&other, None, "user:master")
        .await
        .unwrap();
    // `other` was cancelled by its own request: the parent's record does
    // not leak into its answer, nor the reverse.
    assert_eq!(
        ControlHandle::cancel(&h.ctl, &other, None, "user:master")
            .await
            .unwrap(),
        vec![other.clone()]
    );
    assert_eq!(
        ControlHandle::cancel(&h.ctl, &parent, None, "user:master")
            .await
            .unwrap(),
        vec![parent]
    );
}

/// A cancel stops the run from memory: with `loop_lock` held by a tick and
/// the store's shared connection held by a long statement, the run is
/// stopped at once, its own and one under a cancelled ancestor alike.
#[tokio::test]
async fn cancel_stops_the_run_while_the_store_and_the_loop_are_both_busy() {
    let (h, worker, id) = running().await;
    let run_id = worker.runs.lock().unwrap()[0].clone();
    let held = h.ctl.loop_lock.lock().await;
    let store_busy = h.store().hold_shared_connection();
    let mut cancel = pin!(ControlHandle::cancel(
        &h.ctl,
        &id,
        Some("not needed".into()),
        "user:master"
    ));
    tokio::select! {
        _ = &mut cancel => panic!("the cancel records under loop_lock"),
        _ = tokio::time::sleep(Duration::from_millis(200)) => {}
    }
    assert_eq!(*worker.stopped.lock().unwrap(), vec![run_id]);
    assert!(h.ctl.running().is_empty(), "stopped before any store read");
    drop(store_busy);
    drop(held);
    cancel.await.expect("cancel");
    assert!(matches!(h.status(&id).await, Status::Cancelled(_)));
}

#[tokio::test]
async fn cancelling_an_ancestor_stops_the_run_from_memory() {
    let (h, worker, id) = running().await;
    let run_id = worker.runs.lock().unwrap()[0].clone();
    h.ctl.state().runs.get_mut(&id).unwrap().ancestors = vec!["parent".into(), "root".into()];
    let held = h.ctl.loop_lock.lock().await;
    let store_busy = h.store().hold_shared_connection();
    let mut cancel = pin!(ControlHandle::cancel(&h.ctl, "root", None, "user:master"));
    tokio::select! {
        _ = &mut cancel => panic!("the cancel records under loop_lock"),
        _ = tokio::time::sleep(Duration::from_millis(200)) => {}
    }
    assert_eq!(*worker.stopped.lock().unwrap(), vec![run_id]);
    assert!(h.ctl.state().cancelling.contains(&id));
    drop(store_busy);
    drop(held);
    // "root" is not a real item: the cancel is refused, and the marks lift.
    assert!(cancel.await.is_err());
    assert!(h.ctl.state().cancelling.is_empty());
}

/// With the shared connection held, a heartbeat write waits, but the
/// heartbeat loop does not: it still ends with its run. (Before, the loop
/// awaited the write and stayed stuck until the connection was free.)
#[tokio::test]
async fn the_heartbeat_loop_never_waits_on_a_busy_store() {
    let (h, worker, id) = running().await;
    let store_busy = h.store().hold_shared_connection();
    worker.pulse.store(1, Ordering::SeqCst);
    // Several intervals (250 ms each) with activity while the write waits.
    for n in 2..6 {
        tokio::time::sleep(Duration::from_millis(300)).await;
        worker.pulse.store(n, Ordering::SeqCst);
    }
    h.ctl.state().runs[&id].handle.abort();
    wait_until("the heartbeat task ends with its run", || {
        h.ctl.state().runs[&id]
            ._heartbeat
            .0
            .as_ref()
            .is_some_and(|task| task.is_finished())
    })
    .await;
    drop(store_busy);
}
