//! Phase 5 in the loop: a peer's run outlives a controller restart and is
//! re-attached, one its node no longer holds returns to `ready`, and a stop
//! reaches the node.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rustykrab_core::work::{EventKind, ResultReport, Status, WorkerKind};
use rustykrab_core::Error;

use super::{done, draft, Harness};
use crate::controller::{Controller, ControllerConfig};
use crate::handle::ControlHandle;
use crate::routing::RecordRouting;
use crate::worker::{Brief, Worker, WorkerCapabilities};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Task {
    Running,
    Done,
    Cancelled,
}

/// A peer's node: its tasks by run id, which outlive any controller.
#[derive(Default)]
struct Node {
    tasks: Mutex<HashMap<String, Task>>,
    submissions: AtomicUsize,
}

impl Node {
    fn finish_all(&self) {
        for task in self.tasks.lock().unwrap().values_mut() {
            if *task == Task::Running {
                *task = Task::Done;
            }
        }
    }

    fn forget_all(&self) {
        self.tasks.lock().unwrap().clear();
    }

    fn state(&self, run: &str) -> Option<Task> {
        self.tasks.lock().unwrap().get(run).copied()
    }
}

/// A peer worker over a [`Node`]: a submission is idempotent by run id, a
/// run polls its task, `resumable` asks the node, `stop` cancels there.
struct Peer {
    node: Arc<Node>,
}

#[async_trait]
impl Worker for Peer {
    fn name(&self) -> &str {
        "krabby"
    }

    fn kind(&self) -> WorkerKind {
        WorkerKind::Peer
    }

    fn capabilities(&self) -> WorkerCapabilities {
        WorkerCapabilities {
            tools: vec!["*".to_string()],
            writable_resources: vec!["*".to_string()],
            ..WorkerCapabilities::default()
        }
    }

    async fn run(&self, brief: Brief) -> Result<ResultReport, Error> {
        let run = brief.run.clone().expect("the controller names every run");
        {
            let mut tasks = self.node.tasks.lock().unwrap();
            if !tasks.contains_key(&run) {
                tasks.insert(run.clone(), Task::Running);
                self.node.submissions.fetch_add(1, Ordering::SeqCst);
            }
        }
        loop {
            match self.node.state(&run) {
                Some(Task::Done) => return Ok(done(&brief.title)),
                Some(Task::Cancelled) | None => {
                    return Err(Error::Internal("the node dropped the task".into()))
                }
                Some(Task::Running) => {
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await
                }
            }
        }
    }

    async fn resumable(&self, run: &str) -> bool {
        matches!(self.node.state(run), Some(Task::Running | Task::Done))
    }

    fn stop(&self, run: &str) {
        self.node
            .tasks
            .lock()
            .unwrap()
            .insert(run.to_string(), Task::Cancelled);
    }
}

fn fleet(node: &Arc<Node>) -> Vec<Arc<dyn Worker>> {
    vec![Arc::new(Peer {
        node: Arc::clone(node),
    })]
}

impl Harness {
    /// The same store and clock under a fresh controller over `fleet`, as
    /// after the controller's daemon restarted. The old controller is
    /// dropped first, which drops its runs without stopping them.
    fn restart_with(self, fleet: Vec<Arc<dyn Worker>>) -> Harness {
        let Harness {
            ctl,
            clock,
            script,
            store,
            config,
            catalog,
            dir,
        } = self;
        drop(ctl);
        let ctl = Controller::new(store.clone(), fleet, config.clone())
            .with_clock(clock.clone())
            .with_catalog(catalog.clone());
        let routing = Arc::new(RecordRouting::new(ctl.registry().clone()));
        Harness {
            ctl: ctl.with_routing(routing),
            clock,
            script,
            store,
            config,
            catalog,
            dir,
        }
    }
}

async fn leased_to_the_peer(h: &Harness, title: &str) -> String {
    let id = h.file_one(draft("a", title)).await;
    h.step().await;
    assert_eq!(h.status(&id).await, Status::Running);
    id
}

#[tokio::test]
async fn a_peers_run_survives_the_controllers_restart() {
    let node = Arc::new(Node::default());
    let h = Harness::with_fleet(ControllerConfig::default(), fleet(&node));
    let id = leased_to_the_peer(&h, "Sort the photo library").await;
    assert_eq!(node.submissions.load(Ordering::SeqCst), 1);

    let h = h.restart_with(fleet(&node));
    h.step().await;
    assert_eq!(
        h.status(&id).await,
        Status::Running,
        "the lease stands while the node holds the run"
    );
    let events = h.events(&id).await;
    let resumed = events
        .iter()
        .position(|e| {
            e.kind == EventKind::Resume
                && e.reason
                    .as_deref()
                    .is_some_and(|r| r.contains("re-attached"))
        })
        .unwrap_or_else(|| panic!("no re-attach event: {events:?}"));
    let leased = events
        .iter()
        .rposition(|e| e.kind == EventKind::Lease)
        .expect("a lease");
    assert!(
        !events[leased..].iter().any(|e| e.to == Some(Status::Ready)) && resumed > leased,
        "nothing went back to ready: {events:?}"
    );

    node.finish_all();
    h.drain().await;
    assert_eq!(h.status(&id).await, Status::Done);
    assert_eq!(
        node.submissions.load(Ordering::SeqCst),
        1,
        "the re-attached run is the same task, not a second one"
    );
    assert!(!h
        .events(&id)
        .await
        .iter()
        .any(|e| e.to == Some(Status::Failed)));
}

#[tokio::test]
async fn a_run_the_peer_lost_returns_to_ready_and_runs_again() {
    let node = Arc::new(Node::default());
    let h = Harness::with_fleet(ControllerConfig::default(), fleet(&node));
    let id = leased_to_the_peer(&h, "Renew the parking permit").await;

    node.forget_all();
    let h = h.restart_with(fleet(&node));
    h.tick().await;
    let events = h.events(&id).await;
    assert!(
        events
            .iter()
            .any(|e| e.to == Some(Status::Ready) && e.kind == EventKind::Resume),
        "{events:?}"
    );
    h.step().await;
    node.finish_all();
    h.drain().await;
    assert_eq!(h.status(&id).await, Status::Done);
    assert_eq!(node.submissions.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn cancelling_an_item_stops_the_peers_task() {
    let node = Arc::new(Node::default());
    let h = Harness::with_fleet(ControllerConfig::default(), fleet(&node));
    let id = leased_to_the_peer(&h, "Book the ferry").await;
    let run = node
        .tasks
        .lock()
        .unwrap()
        .keys()
        .next()
        .cloned()
        .expect("a task");

    ControlHandle::cancel(&h.ctl, &id, None, "user")
        .await
        .unwrap();
    assert_eq!(node.state(&run), Some(Task::Cancelled));
}
