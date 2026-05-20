use crate::discovery::group_by_class;
use crate::fork_pool::PhpForkPool;
use crate::types::{BatchClass, BatchPlan, TestCase, TestOutcome, TestStatus};
use anyhow::Result;
use serde::Deserialize;
use std::collections::{HashMap, VecDeque};
use std::io::BufRead;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;

#[derive(Debug, Clone)]
pub struct RunConfig {
    pub autoload:  PathBuf,
    pub bootstrap: Option<PathBuf>,
    pub filter:    Option<String>,
    pub defines:   Vec<[String; 2]>,
}

#[derive(Debug)]
pub struct Report {
    pub outcomes:          Vec<TestOutcome>,
    pub total_duration_ms: f64,
}

impl Report {
    pub fn count(&self, status: TestStatus) -> usize {
        self.outcomes.iter().filter(|o| o.status == status).count()
    }
    pub fn passed(&self)     -> usize { self.count(TestStatus::Pass) }
    pub fn failed(&self)     -> usize { self.count(TestStatus::Fail) }
    pub fn errored(&self)    -> usize { self.count(TestStatus::Error) }
    pub fn skipped(&self)    -> usize { self.count(TestStatus::Skipped) }
    pub fn incomplete(&self) -> usize { self.count(TestStatus::Incomplete) }
    pub fn risky(&self)      -> usize { self.count(TestStatus::Risky) }

    pub fn is_success(&self) -> bool {
        self.failed() == 0 && self.errored() == 0
    }
}

/// One per-line message from a worker child: either a test outcome or a
/// `batch_done` ready-signal. Untagged so we can deserialize without changing
/// the existing TestOutcome JSON shape.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum WorkerMessage {
    BatchDone {
        #[allow(dead_code)] // discriminant only — its truthiness is the signal
        batch_done: bool,
    },
    Outcome(TestOutcome),
}

enum WorkerEvent {
    Message(usize, WorkerMessage),
    Eof(usize),
}

/// Run all cases through the fork pool using a work-stealing dispatcher.
///
/// Each class is one dispatch unit. The master sends one initial chunk per
/// slot, then pushes the next class to whichever worker reports `batch_done`
/// first. When the queue empties, idle workers' stdin pipes are closed and
/// they exit on EOF.
pub fn run(
    pool: &mut PhpForkPool,
    cases: Vec<TestCase>,
    cfg: &RunConfig,
    on_progress: impl Fn(&TestOutcome) + Sync,
) -> Result<Report> {
    let filtered: Vec<TestCase> = cases.into_iter()
        .filter(|c| match &cfg.filter {
            Some(f) => format!("{}::{}", c.class, c.method).contains(f.as_str()),
            None    => true,
        })
        .collect();

    let n = pool.len();
    let mut queue: VecDeque<BatchPlan> = build_queue(filtered, cfg);

    // Spawn one reader thread per slot. Each owns its BufReader and forwards
    // parsed messages + EOF over a single mpsc to the main loop below.
    let (tx, rx) = mpsc::channel::<WorkerEvent>();
    let mut reader_handles: Vec<thread::JoinHandle<()>> = Vec::with_capacity(n);
    for slot in 0..n {
        let reader = pool.take_reader(slot)
            .ok_or_else(|| anyhow::anyhow!("read end for slot {slot} missing"))?;
        let tx_slot = tx.clone();
        reader_handles.push(thread::spawn(move || {
            let mut reader = reader;
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => { let _ = tx_slot.send(WorkerEvent::Eof(slot)); break; }
                    Ok(_) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() { continue; }
                        if let Ok(msg) = serde_json::from_str::<WorkerMessage>(trimmed) {
                            if tx_slot.send(WorkerEvent::Message(slot, msg)).is_err() {
                                break;
                            }
                        }
                    }
                    Err(_) => { let _ = tx_slot.send(WorkerEvent::Eof(slot)); break; }
                }
            }
        }));
    }
    drop(tx); // only reader threads hold senders now

    // Seed each slot with one chunk, then close any slots we can't feed.
    let mut slot_busy = vec![false; n];
    let mut slot_open = vec![true; n];
    for slot in 0..n {
        match queue.pop_front() {
            Some(plan) => {
                pool.write_batch(slot, &plan)?;
                slot_busy[slot] = true;
            }
            None => {
                pool.close_slot(slot);
                slot_open[slot] = false;
            }
        }
    }

    // Main dispatcher loop.
    let mut outcomes: Vec<TestOutcome> = Vec::new();
    let mut total = 0.0f64;
    let mut live_readers = n;
    while live_readers > 0 {
        let ev = match rx.recv() {
            Ok(e) => e,
            Err(_) => break,
        };
        match ev {
            WorkerEvent::Message(_slot, WorkerMessage::Outcome(o)) => {
                total += o.duration_ms;
                on_progress(&o);
                outcomes.push(o);
            }
            WorkerEvent::Message(slot, WorkerMessage::BatchDone { .. }) => {
                slot_busy[slot] = false;
                if let Some(plan) = queue.pop_front() {
                    pool.write_batch(slot, &plan)?;
                    slot_busy[slot] = true;
                } else if slot_open[slot] {
                    pool.close_slot(slot);
                    slot_open[slot] = false;
                }
            }
            WorkerEvent::Eof(_slot) => {
                live_readers -= 1;
            }
        }
    }

    for h in reader_handles { let _ = h.join(); }
    pool.wait();

    Ok(Report { outcomes, total_duration_ms: total })
}

/// Build the work queue using LPT (Longest Processing Time) scheduling:
/// classes with the most methods go first so workers tackle the heaviest
/// items while the queue is still wide enough to keep them all fed.
///
/// Adaptive chunking limits RPC overhead on tiny classes: heavy classes
/// (>= HEAVY_METHOD_THRESHOLD methods) get a plan each for max parallelism,
/// while light classes are bundled into packs to amortize the JSON round-trip.
fn build_queue(cases: Vec<TestCase>, cfg: &RunConfig) -> VecDeque<BatchPlan> {
    const HEAVY_METHOD_THRESHOLD: usize = 4;
    const LIGHT_PACK_SIZE:        usize = 4;

    let mut groups = group_by_class(cases);
    // LPT: heaviest classes first. method_count is our static cost proxy —
    // cheap, derived from discovery without any extra parse, and a strict
    // overestimate for short methods is fine (the worker doesn't care).
    groups.sort_by(|a, b| b.methods.len().cmp(&a.methods.len()));

    let mut queue: VecDeque<BatchPlan> = VecDeque::with_capacity(groups.len());
    let mut light_buf: Vec<BatchClass> = Vec::with_capacity(LIGHT_PACK_SIZE);

    let mk_plan = |classes: Vec<BatchClass>| BatchPlan {
        autoload:  cfg.autoload.clone(),
        bootstrap: cfg.bootstrap.clone(),
        defines:   cfg.defines.clone(),
        classes,
    };

    for g in groups {
        let bc = BatchClass { file: g.file, class: g.class, methods: g.methods };
        if bc.methods.len() >= HEAVY_METHOD_THRESHOLD {
            queue.push_back(mk_plan(vec![bc]));
        } else {
            light_buf.push(bc);
            if light_buf.len() >= LIGHT_PACK_SIZE {
                queue.push_back(mk_plan(std::mem::take(&mut light_buf)));
            }
        }
    }
    if !light_buf.is_empty() {
        queue.push_back(mk_plan(light_buf));
    }
    queue
}

/// Distribute `cases` across `n` slots without splitting any class.
/// Kept for any external callers; the runner itself no longer needs it.
pub fn chunk_by_class(cases: Vec<TestCase>, n: usize) -> Vec<Vec<TestCase>> {
    let n = n.max(1);
    let mut class_order: Vec<String> = Vec::new();
    let mut by_class: HashMap<String, Vec<TestCase>> = HashMap::new();
    for c in cases {
        by_class.entry(c.class.clone())
            .or_insert_with(|| { class_order.push(c.class.clone()); Vec::new() })
            .push(c);
    }
    let mut slots: Vec<Vec<TestCase>> = vec![Vec::new(); n];
    for (i, class) in class_order.into_iter().enumerate() {
        if let Some(class_cases) = by_class.remove(&class) {
            slots[i % n].extend(class_cases);
        }
    }
    slots
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_case(class: &str, method: &str) -> TestCase {
        TestCase {
            file:   PathBuf::from("/f.php"),
            class:  class.to_string(),
            method: method.to_string(),
        }
    }

    #[test]
    fn chunk_preserves_all_cases() {
        let cases = vec![
            make_case("A", "t1"), make_case("A", "t2"),
            make_case("B", "t1"),
            make_case("C", "t1"),
        ];
        let chunks = chunk_by_class(cases, 2);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks.iter().map(|c| c.len()).sum::<usize>(), 4);
    }

    #[test]
    fn chunk_never_splits_a_class() {
        let cases = vec![
            make_case("Alpha", "t1"), make_case("Alpha", "t2"), make_case("Alpha", "t3"),
            make_case("Beta", "t1"),
        ];
        let chunks = chunk_by_class(cases, 3);
        for chunk in &chunks {
            let alpha = chunk.iter().filter(|c| c.class == "Alpha").count();
            assert!(alpha == 0 || alpha == 3,
                "Alpha was split: found {alpha} in one slot");
        }
    }

    #[test]
    fn chunk_handles_more_slots_than_classes() {
        let cases = vec![make_case("Only", "t1")];
        let chunks = chunk_by_class(cases, 8);
        assert_eq!(chunks.len(), 8);
        assert_eq!(chunks.iter().filter(|c| !c.is_empty()).count(), 1);
    }

    #[test]
    fn build_queue_packs_light_classes_and_sorts_heavy_first() {
        // 1 heavy class (5 methods) + 5 light classes (1 method each).
        // Expected: heavy plan first, then 1 pack of 4 light + 1 pack of 1 light.
        let mut cases = Vec::new();
        for m in 0..5 { cases.push(make_case("Heavy", &format!("t{m}"))); }
        for c in 0..5 { cases.push(make_case(&format!("Light{c}"), "t1")); }
        let cfg = RunConfig {
            autoload:  PathBuf::from("/autoload.php"),
            bootstrap: None,
            filter:    None,
            defines:   vec![],
        };
        let q: Vec<_> = build_queue(cases, &cfg).into_iter().collect();
        assert_eq!(q.len(), 3, "1 heavy plan + 2 light packs");
        assert_eq!(q[0].classes.len(), 1);
        assert_eq!(q[0].classes[0].class, "Heavy", "heavy class scheduled first");
        assert_eq!(q[1].classes.len(), 4, "first light pack is full");
        assert_eq!(q[2].classes.len(), 1, "remaining light tail in its own pack");
    }
}
