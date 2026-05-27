use crate::discovery::group_by_class;
use crate::fork_pool::PhpForkPool;
use crate::provider_enum::RowCounts;
use crate::types::{BatchClass, BatchPlan, RowFilter, TestCase, TestOutcome, TestStatus};
use anyhow::Result;
use serde::Deserialize;
use std::collections::{HashMap, VecDeque};
use std::io::BufRead;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;

/// Stopping policy applied at runtime by the dispatcher: when an outcome
/// matching one of these statuses arrives, the queue is drained and no
/// further plans are sent. In-flight tests on other workers still finish.
#[derive(Debug, Clone, Copy, Default)]
pub struct StopOn {
    pub failure:    bool,
    pub error:      bool,
    pub skipped:    bool,
    pub incomplete: bool,
    pub risky:      bool,
}

impl StopOn {
    /// `--stop-on-failure` PHPUnit semantics: stop on Fail OR Error
    /// (both are "real" defects).
    pub fn on_failure() -> Self {
        Self { failure: true, error: true, ..Self::default() }
    }
    /// `--stop-on-defect` PHPUnit semantics: stop on anything not-pass.
    pub fn on_defect() -> Self {
        Self {
            failure: true, error: true,
            skipped: true, incomplete: true, risky: true,
        }
    }
    pub fn matches(&self, status: &TestStatus) -> bool {
        match status {
            TestStatus::Fail       => self.failure,
            TestStatus::Error      => self.error,
            TestStatus::Skipped    => self.skipped,
            TestStatus::Incomplete => self.incomplete,
            TestStatus::Risky      => self.risky,
            TestStatus::Pass       => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RunConfig {
    pub autoload:  PathBuf,
    pub bootstrap: Option<PathBuf>,
    pub filter:    Option<String>,
    pub defines:   Vec<[String; 2]>,
    pub stop_on:   StopOn,
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
    row_counts: &RowCounts,
    on_progress: impl Fn(&TestOutcome) + Sync,
) -> Result<Report> {
    let filtered: Vec<TestCase> = cases.into_iter()
        .filter(|c| match &cfg.filter {
            Some(f) => format!("{}::{}", c.class, c.method).contains(f.as_str()),
            None    => true,
        })
        .collect();

    let n = pool.len();
    let mut queue: VecDeque<BatchPlan> = build_queue(filtered, cfg, row_counts);

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
    let mut stopping = false;
    while live_readers > 0 {
        let ev = match rx.recv() {
            Ok(e) => e,
            Err(_) => break,
        };
        match ev {
            WorkerEvent::Message(_slot, WorkerMessage::Outcome(o)) => {
                total += o.duration_ms;
                on_progress(&o);
                // --stop-on-X policy: if this outcome matches, drain the
                // queue so no further plans are dispatched. In-flight
                // tests on other workers finish naturally; their EOF
                // closes out the loop.
                if !stopping && cfg.stop_on.matches(&o.status) {
                    stopping = true;
                    queue.clear();
                }
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

/// Static weight of one test method for LPT cost estimation.
///
/// We deliberately use method *count* (every method = 1) rather than
/// enumerated row count: tried weighting by row count first, it pushed
/// nearly every dp-containing class above the heavy threshold and
/// over-individualised the queue, regressing carbon by 20%. The row
/// count is still consulted — but only to decide whether to *split*
/// a heavy method into per-row plans (see ROW_SPLIT_THRESHOLD), not
/// to bias the LPT ordering.
fn method_weight(_class: &str, _m: &crate::discovery::GroupedMethod, _row_counts: &RowCounts) -> u32 {
    1
}

/// A method with this many enumerated data-provider rows or more gets
/// split into stride-partitioned plans. Set high because every chunk pays
/// the full setUpBeforeClass/tearDownAfterClass cost; below ~15 rows the
/// duplicated class setup outweighs the parallelism gain. (Tried 2 first:
/// regressed carbon by 25% — the empirically dumb-but-wrong knob.)
const ROW_SPLIT_THRESHOLD: u32 = 15;
/// Max chunks a single method is split into. Bounded so we don't fragment
/// a 1000-row provider into 1000 single-row plans (each pays setUpBeforeClass).
const MAX_ROW_CHUNKS: u32 = 4;

/// Build the work queue. Three categories of work:
///
/// 1. **Heavy data-provider methods** (>= ROW_SPLIT_THRESHOLD rows): split
///    the class into N plans, each with `methods: [thatMethod]` and a
///    `row_filter` selecting rows where `i % N == chunk_index`. Other
///    methods of the same class get a separate filterless plan.
///
/// 2. **Other heavy classes** (cost >= HEAVY_COST_THRESHOLD): one plan per
///    class for maximum parallelism.
///
/// 3. **Light classes**: bundled in packs of LIGHT_PACK_SIZE to amortize
///    the JSON round-trip cost.
///
/// LPT ordering applies across all three: heaviest first into the queue.
fn build_queue(cases: Vec<TestCase>, cfg: &RunConfig, row_counts: &RowCounts) -> VecDeque<BatchPlan> {
    const HEAVY_COST_THRESHOLD: u32   = 4;
    const LIGHT_PACK_SIZE:      usize = 4;

    let groups = group_by_class(cases);
    let mut by_cost: Vec<(u32, crate::discovery::TestClass)> = groups.into_iter()
        .map(|g| {
            let cost: u32 = g.methods.iter()
                .map(|m| method_weight(&g.class, m, row_counts))
                .sum();
            (cost, g)
        })
        .collect();
    by_cost.sort_by(|a, b| b.0.cmp(&a.0));

    let mut queue: VecDeque<BatchPlan> = VecDeque::with_capacity(by_cost.len());
    let mut light_buf: Vec<BatchClass> = Vec::with_capacity(LIGHT_PACK_SIZE);

    let mk_plan = |classes: Vec<BatchClass>| BatchPlan {
        autoload:  cfg.autoload.clone(),
        bootstrap: cfg.bootstrap.clone(),
        defines:   cfg.defines.clone(),
        classes,
    };

    for (cost, g) in by_cost {
        // Only split methods where the enumerator gave us an actual row
        // count. Unknown (or unresolved) providers stay in the plain plan —
        // splitting blind risks creating empty chunks (filter chunk_index
        // > actual rows-1 keeps nothing) which is wasted work.
        let row_count_for = |m: &crate::discovery::GroupedMethod| -> Option<u32> {
            let dp = m.data_provider.as_ref()?;
            match row_counts.get(&(g.class.clone(), dp.clone())) {
                Some(Some(n)) => Some(*n as u32),
                _             => None,
            }
        };

        let (heavy_methods, other_methods): (Vec<_>, Vec<_>) = g.methods.iter().cloned()
            .partition(|m| row_count_for(m).map(|n| n >= ROW_SPLIT_THRESHOLD).unwrap_or(false));

        for hm in &heavy_methods {
            let rows = row_count_for(hm).unwrap_or(1);
            // chunks = min(rows, MAX_ROW_CHUNKS) so a 2-row method splits
            // into 2 (each gets one row) and a 100-row method splits into
            // MAX_ROW_CHUNKS (each gets ~25). No empty chunks.
            let chunks = rows.min(MAX_ROW_CHUNKS);
            for chunk_index in 0..chunks {
                let bc = BatchClass {
                    file:       g.file.clone(),
                    class:      g.class.clone(),
                    methods:    vec![hm.name.clone()],
                    row_filter: Some(RowFilter { chunk_index, total_chunks: chunks }),
                };
                queue.push_back(mk_plan(vec![bc]));
            }
        }

        // Remaining (non-row-split) methods form their own batch with no filter.
        if other_methods.is_empty() { continue; }
        let other_cost = cost.saturating_sub(
            heavy_methods.iter().map(|m| method_weight(&g.class, m, row_counts)).sum::<u32>()
        );
        let other_names: Vec<String> = other_methods.into_iter().map(|m| m.name).collect();
        let bc = BatchClass {
            file:       g.file,
            class:      g.class,
            methods:    other_names,
            row_filter: None,
        };
        if other_cost >= HEAVY_COST_THRESHOLD {
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
            file:               PathBuf::from("/f.php"),
            class:              class.to_string(),
            method:             method.to_string(),
            data_provider:      None,
            groups:             vec![],
            external_providers: vec![],
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

    fn make_case_dp(class: &str, method: &str, dp: &str) -> TestCase {
        TestCase {
            file:               PathBuf::from("/f.php"),
            class:              class.to_string(),
            method:             method.to_string(),
            data_provider:      Some(dp.to_string()),
            groups:             vec![],
            external_providers: vec![],
        }
    }

    #[test]
    fn build_queue_splits_heavy_provider_into_stride_chunks() {
        // One class with one method that has 20 dp rows, plus one plain
        // method on the same class. Expect: 4 row-split plans (chunks=4,
        // because 20/4 ceil = 5, clamped to MAX_ROW_CHUNKS=4) + 1 plan
        // for the plain method.
        let cases = vec![
            make_case_dp("BigDp", "testFat", "provideMany"),
            make_case("BigDp", "testSimple"),
        ];
        let cfg = RunConfig {
            autoload:  PathBuf::from("/autoload.php"),
            bootstrap: None,
            filter:    None,
            defines:   vec![],
            stop_on:   StopOn::default(),
        };
        let mut row_counts = RowCounts::new();
        row_counts.insert(("BigDp".to_string(), "provideMany".to_string()), Some(20));

        let q: Vec<_> = build_queue(cases, &cfg, &row_counts).into_iter().collect();
        let row_split_plans: Vec<_> = q.iter()
            .filter(|p| p.classes.iter().any(|c| c.row_filter.is_some()))
            .collect();
        assert_eq!(row_split_plans.len(), 4, "20 rows split into 4 chunks");
        // Each chunk gets a unique chunk_index 0..4 with total_chunks=4.
        let indices: std::collections::BTreeSet<u32> = row_split_plans.iter()
            .flat_map(|p| p.classes.iter().filter_map(|c| c.row_filter.as_ref().map(|f| f.chunk_index)))
            .collect();
        assert_eq!(indices, [0, 1, 2, 3].iter().copied().collect());
        for p in &row_split_plans {
            let f = p.classes[0].row_filter.as_ref().unwrap();
            assert_eq!(f.total_chunks, 4);
            assert_eq!(p.classes[0].methods, vec!["testFat".to_string()]);
        }
        // And one filterless plan with the remaining method.
        let plain_plans: Vec<_> = q.iter()
            .filter(|p| p.classes.iter().all(|c| c.row_filter.is_none()))
            .collect();
        assert_eq!(plain_plans.len(), 1);
        assert!(plain_plans[0].classes[0].methods.contains(&"testSimple".to_string()));
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
            stop_on:   StopOn::default(),
        };
        let row_counts = RowCounts::new();
        let q: Vec<_> = build_queue(cases, &cfg, &row_counts).into_iter().collect();
        assert_eq!(q.len(), 3, "1 heavy plan + 2 light packs");
        assert_eq!(q[0].classes.len(), 1);
        assert_eq!(q[0].classes[0].class, "Heavy", "heavy class scheduled first");
        assert_eq!(q[1].classes.len(), 4, "first light pack is full");
        assert_eq!(q[2].classes.len(), 1, "remaining light tail in its own pack");
    }

}
