//! Workers whose runs wait on one exclusive resource ([`Worker::resource_slot`])
//! are leased one item at a time between them, whatever models they name.
//! A run queued behind the resource can show no activity, so leasing it
//! would only have the stall detector fail it.

use super::*;
use std::sync::atomic::AtomicUsize;

/// A local worker naming `model`, waiting on the resource `slot`; each run
/// holds the slot until the test opens the gate once for it.
struct SlotWorker {
    name: &'static str,
    model: &'static str,
    slot: Option<&'static str>,
    gate: Arc<tokio::sync::Semaphore>,
    /// Runs in flight across every worker sharing this counter, and the
    /// most ever at once.
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    seen: Mutex<Vec<String>>,
}

impl SlotWorker {
    fn new(
        name: &'static str,
        model: &'static str,
        slot: Option<&'static str>,
        gate: &Arc<tokio::sync::Semaphore>,
        active: &Arc<AtomicUsize>,
        peak: &Arc<AtomicUsize>,
    ) -> Arc<Self> {
        Arc::new(Self {
            name,
            model,
            slot,
            gate: gate.clone(),
            active: active.clone(),
            peak: peak.clone(),
            seen: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl Worker for SlotWorker {
    fn name(&self) -> &str {
        self.name
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::Local
    }
    fn capabilities(&self) -> WorkerCapabilities {
        WorkerCapabilities {
            models: vec![self.model.to_string()],
            writable_resources: vec!["*".to_string()],
            ..WorkerCapabilities::default()
        }
    }
    fn resource_slot(&self) -> Option<String> {
        self.slot.map(str::to_string)
    }
    async fn run(&self, brief: Brief) -> Result<ResultReport, Error> {
        self.seen.lock().unwrap().push(brief.title.clone());
        let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        let permit = self.gate.acquire().await.expect("gate open");
        permit.forget();
        self.active.fetch_sub(1, Ordering::SeqCst);
        Ok(done(&brief.title))
    }
}

struct Fleet {
    h: Harness,
    gate: Arc<tokio::sync::Semaphore>,
    peak: Arc<AtomicUsize>,
    workers: Vec<Arc<SlotWorker>>,
}

fn fleet(specs: &[(&'static str, &'static str, Option<&'static str>)]) -> Fleet {
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let workers: Vec<Arc<SlotWorker>> = specs
        .iter()
        .map(|(name, model, slot)| SlotWorker::new(name, model, *slot, &gate, &active, &peak))
        .collect();
    let h = Harness::with_fleet(
        ControllerConfig::default(),
        workers
            .iter()
            .map(|w| w.clone() as Arc<dyn Worker>)
            .collect(),
    );
    Fleet {
        h,
        gate,
        peak,
        workers,
    }
}

async fn two_items(h: &Harness) -> (String, String) {
    let a = h.file_one(draft("a", "First job")).await;
    let b = h.file_one(draft("b", "Second job")).await;
    (a, b)
}

#[tokio::test]
async fn workers_sharing_a_slot_under_different_model_labels_are_leased_one_at_a_time() {
    // The daemon's planner advertised its provider's name and the coding
    // worker its model tag, while both waited on one Ollama slot.
    let f = fleet(&[
        ("planner", "ollama", Some("model:daemon")),
        ("qwen-code", "qwen3.8:27b-mlx", Some("model:daemon")),
    ]);
    let (a, b) = two_items(&f.h).await;
    let first = f.h.tick().await;
    assert_eq!(
        first.leased.len(),
        1,
        "two ready items in one pass go into one slot at most: {:?}",
        first.leased
    );
    for _ in 0..3 {
        assert!(
            f.h.step().await.leased.is_empty(),
            "nothing else is leased while the slot is held"
        );
    }
    let waiting = if first.leased[0] == a { &b } else { &a };
    assert_eq!(f.h.status(waiting).await, Status::Ready);
    // The first run completes; only then is the second leased.
    f.gate.add_permits(1);
    let mut leased = Vec::new();
    for _ in 0..10 {
        leased.extend(f.h.step().await.leased);
        if !leased.is_empty() {
            break;
        }
    }
    assert_eq!(leased, vec![waiting.clone()]);
    assert_eq!(f.h.status(&first.leased[0]).await, Status::Done);
    f.gate.add_permits(1);
    f.h.drain_settled().await;
    assert_eq!(f.h.status(&a).await, Status::Done);
    assert_eq!(f.h.status(&b).await, Status::Done);
    assert_eq!(f.peak.load(Ordering::SeqCst), 1, "never two runs at once");
}

#[tokio::test]
async fn a_probe_holding_the_slot_blocks_leasing_to_every_worker_on_it() {
    let f = fleet(&[
        ("planner", "ollama", Some("model:daemon")),
        ("qwen-code", "qwen3.8:27b-mlx", Some("model:daemon")),
    ]);
    let id = f.h.file_one(draft("job", "Only job")).await;
    f.h.ctl.state().probing.insert("planner".to_string(), 1);
    {
        let code: &dyn Worker = f.workers[1].as_ref();
        assert!(
            !f.h.ctl.has_free_slot(code),
            "planner's probe holds qwen-code's slot"
        );
    }
    for _ in 0..3 {
        assert!(f.h.tick().await.leased.is_empty());
    }
    assert_eq!(f.h.status(&id).await, Status::Ready);
    f.h.ctl.state().probing.clear();
    assert_eq!(f.h.tick().await.leased, vec![id.clone()]);
    f.gate.add_permits(1);
    f.h.drain_settled().await;
    assert_eq!(f.h.status(&id).await, Status::Done);
}

#[tokio::test]
async fn workers_without_a_slot_or_on_different_slots_still_run_side_by_side() {
    for specs in [
        [("planner", "model-a", None), ("qwen-code", "model-b", None)],
        [
            ("planner", "model-a", Some("model:daemon")),
            ("qwen-code", "model-b", Some("model:local-resources")),
        ],
    ] {
        let f = fleet(&specs);
        let (a, b) = two_items(&f.h).await;
        let mut leased = f.h.tick().await.leased;
        leased.sort();
        let mut both = vec![a.clone(), b.clone()];
        both.sort();
        assert_eq!(leased, both, "{specs:?}");
        f.gate.add_permits(2);
        f.h.drain_settled().await;
        assert_eq!(f.peak.load(Ordering::SeqCst), 2);
        assert!(f.workers.iter().all(|w| w.seen.lock().unwrap().len() == 1));
    }
}
