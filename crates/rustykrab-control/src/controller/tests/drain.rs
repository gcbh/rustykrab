//! Draining for shutdown: a draining controller leases nothing new but
//! still reconciles, and a run the host interrupted returns its item to
//! `ready` without climbing the ladder.

use super::*;
use crate::worker::RunFailure;

fn interrupted() -> Error {
    RunFailure::Interrupted {
        detail: "the daemon shut down".to_string(),
    }
    .into_error()
}

#[tokio::test]
async fn a_draining_controller_leases_nothing() {
    let h = Harness::new(&["pinch"]);
    let status = ControlHandle::loop_status(&h.ctl).expect("the controller runs a loop");
    assert!(!status.draining);

    ControlHandle::set_draining(&h.ctl, true);
    let x = h.file_one(draft("x", "Tidy the notes")).await;
    let report = h.tick().await;
    assert!(report.leased.is_empty(), "{report:?}");
    assert_eq!(h.status(&x).await, Status::Ready);
    assert_eq!(h.leases(&x).await, 0);
    assert!(h.script.briefs().is_empty());
    let status = ControlHandle::loop_status(&h.ctl).unwrap();
    assert!(status.draining);
    assert_eq!(status.runs_in_flight, 0);

    // Draining lifted, the same item leases.
    ControlHandle::set_draining(&h.ctl, false);
    let report = h.tick().await;
    assert_eq!(report.leased, vec![x.clone()]);
    h.drain().await;
    assert_eq!(h.status(&x).await, Status::Done);
}

#[tokio::test]
async fn a_draining_controller_still_reconciles_the_runs_in_flight() {
    let h = Harness::new(&["pinch"]);
    let gate = Arc::new(Notify::new());
    h.script.push(
        "Tidy the notes",
        Step::Wait(gate.clone(), Box::new(done("Tidy the notes"))),
    );
    let x = h.file_one(draft("x", "Tidy the notes")).await;
    let y = h.file_one(draft("y", "Sort the photos")).await;
    let first = h.tick().await;
    assert!(first.leased.contains(&x), "{first:?}");

    ControlHandle::set_draining(&h.ctl, true);
    gate.notify_one();
    let report = h.drain().await;
    assert!(report.leased.is_empty(), "{report:?}");
    assert_eq!(h.status(&x).await, Status::Done);
    if !first.leased.contains(&y) {
        assert_eq!(h.status(&y).await, Status::Ready);
    }
    assert_eq!(
        ControlHandle::loop_status(&h.ctl).unwrap().runs_in_flight,
        0
    );
}

#[tokio::test]
async fn an_interrupted_run_returns_its_item_to_ready_with_no_rung() {
    let h = Harness::new(&["pinch"]);
    h.script.push("Tidy the notes", Step::Fail(interrupted()));
    let x = h.file_one(draft("x", "Tidy the notes")).await;
    let report = h.tick().await;
    assert_eq!(report.leased, vec![x.clone()]);

    // The daemon is on its way down: the tick reconciles and leases nothing.
    ControlHandle::set_draining(&h.ctl, true);
    let report = h.step().await;
    assert_eq!(report.reconciled, vec![x.clone()], "{report:?}");
    assert!(report.leased.is_empty());

    assert_eq!(h.status(&x).await, Status::Ready);
    assert!(
        h.rungs(&x).await.is_empty(),
        "no rung: {:?}",
        h.rungs(&x).await
    );
    assert!(h.store().work_lease_get(&x).await.unwrap().is_none());
    let resumed = h
        .events(&x)
        .await
        .into_iter()
        .rfind(|e| e.kind == EventKind::Resume)
        .expect("a resume event");
    assert_eq!(resumed.to, Some(Status::Ready));
    assert!(
        resumed
            .reason
            .as_deref()
            .is_some_and(|r| r.contains("interrupted")),
        "{resumed:?}"
    );
    assert!(h.outbox().await.is_empty(), "an interruption is no notice");

    // The next daemon runs it again, and it closes with no repair counted.
    ControlHandle::set_draining(&h.ctl, false);
    h.drain().await;
    assert_eq!(h.status(&x).await, Status::Done);
    assert_eq!(h.leases(&x).await, 2);
    assert!(!h.rungs(&x).await.contains(&Rung::Repair));
}

#[tokio::test]
async fn any_other_process_failure_still_climbs_the_ladder() {
    let h = Harness::new(&["pinch"]);
    h.script.push(
        "Tidy the notes",
        Step::Fail(
            RunFailure::Process {
                code: None,
                stderr_tail: "killed".to_string(),
            }
            .into_error(),
        ),
    );
    let x = h.file_one(draft("x", "Tidy the notes")).await;
    h.tick().await;
    ControlHandle::set_draining(&h.ctl, true);
    h.step().await;
    assert!(!h.rungs(&x).await.is_empty());
}
