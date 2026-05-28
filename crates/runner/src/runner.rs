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
    pub autoload:         PathBuf,
    pub bootstrap:        Option<PathBuf>,
    pub filter:           Option<String>,
    pub defines:          Vec<[String; 2]>,
    pub stop_on:          StopOn,
    /// FQCN → file path for all PHP classes in the test roots.
    /// Used to resolve `#[DataProviderExternal]` dependencies.
    pub class_file_index: HashMap<String, PathBuf>,
    /// Number of PHP workers — used to compute adaptive batch sizes.
    pub n_workers:        usize,
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
    let (mut queue, synthetic) = build_queue(filtered, cfg, row_counts);

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

    // Emit synthetic pass outcomes for tautological methods — they are never
    // sent to a PHP worker but must appear in the report to keep test-count
    // parity with vanilla PHPUnit.
    let mut outcomes: Vec<TestOutcome> = Vec::new();
    for o in synthetic {
        on_progress(&o);
        outcomes.push(o);
    }
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
                // If this was the last live reader and there are still
                // batches in the queue, the worker(s) crashed before
                // draining them.  Emit error outcomes so every test
                // appears in the report rather than being silently lost.
                if live_readers == 0 && !queue.is_empty() {
                    while let Some(plan) = queue.pop_front() {
                        for bc in &plan.classes {
                            for method in &bc.methods {
                                let o = TestOutcome {
                                    class:       bc.class.clone(),
                                    method:      method.clone(),
                                    dataset:     None,
                                    status:      TestStatus::Error,
                                    message:     Some(
                                        "worker process crashed before reaching this test".into()
                                    ),
                                    trace:       None,
                                    duration_ms: 0.0,
                                };
                                on_progress(&o);
                                outcomes.push(o);
                            }
                        }
                    }
                }
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

/// Build the work queue.
///
/// Targets a uniform number of test methods per `BatchPlan` so every worker
/// gets roughly the same amount of work regardless of how tests are distributed
/// across classes.
///
/// Algorithm (bin-packing LPT):
///   target = total_methods / (n_workers × OVERSATURATION)
///   Sort classes descending by method count (LPT order).
///   Accumulate classes into the current batch until it reaches `target`;
///   then flush as a BatchPlan and start a new one.
///   Classes with row-split data providers are extracted first and each
///   chunk is still dispatched as a solo plan (fine-grained parallelism).
///   Classes whose non-split method count exceeds `target` are dispatched
///   solo regardless (they are already "full" batches on their own).
///
/// OVERSATURATION ensures more batches than workers so work-stealing has
/// granularity to keep all workers busy even with variance in test duration.
fn build_queue(
    cases: Vec<TestCase>,
    cfg: &RunConfig,
    row_counts: &RowCounts,
) -> (VecDeque<BatchPlan>, Vec<TestOutcome>) {
    const OVERSATURATION: usize = 4; // target 4× more batches than workers

    // Collect required files for a set of method names from the class-file index.
    let required_files_for = |method_names: &[String],
                               all_methods: &[crate::discovery::GroupedMethod]|
     -> Vec<PathBuf> {
        let name_set: std::collections::HashSet<&str> =
            method_names.iter().map(String::as_str).collect();
        let mut files: std::collections::HashSet<PathBuf> = Default::default();
        for gm in all_methods {
            if name_set.is_empty() || name_set.contains(gm.name.as_str()) {
                for (fqcn, _) in &gm.external_providers {
                    if let Some(f) = cfg.class_file_index.get(fqcn) {
                        files.insert(f.clone());
                    }
                }
            }
        }
        files.into_iter().collect()
    };

    let groups = group_by_class(cases);

    // Compute target methods-per-batch: total_methods / (n_workers × OVERSATURATION).
    // Floor at 1 so we never divide by zero; ceil at a large value so a single
    // class with hundreds of methods still gets its own solo plan.
    let total_methods: usize = groups.iter().map(|g| g.methods.len()).sum();
    let n_workers = cfg.n_workers.max(1);
    let target: usize = (total_methods / (n_workers * OVERSATURATION)).max(1);

    // Sort LPT (largest class first) to minimise leftover waste in each bin.
    let mut by_cost: Vec<(u32, crate::discovery::TestClass)> = groups.into_iter()
        .map(|g| {
            let cost: u32 = g.methods.iter()
                .map(|m| method_weight(&g.class, m, row_counts))
                .sum();
            (cost, g)
        })
        .collect();
    by_cost.sort_by(|a, b| b.0.cmp(&a.0));

    let mut queue:      VecDeque<BatchPlan> = VecDeque::with_capacity(by_cost.len());
    let mut synthetic:  Vec<TestOutcome>    = Vec::new();
    let mut bin_buf:    Vec<BatchClass>     = Vec::new();
    let mut bin_methods: usize              = 0;

    let mk_plan = |classes: Vec<BatchClass>| BatchPlan {
        autoload:  cfg.autoload.clone(),
        bootstrap: cfg.bootstrap.clone(),
        defines:   cfg.defines.clone(),
        classes,
    };

    for (cost, g) in by_cost {
        // Extract and dispatch row-split data-provider methods first (unchanged logic).
        let row_count_for = |m: &crate::discovery::GroupedMethod| -> Option<u32> {
            let dp = m.data_provider.as_ref()?;
            match row_counts.get(&(g.class.clone(), dp.clone())) {
                Some(Some(n)) => Some(*n as u32),
                _             => None,
            }
        };

        // Partition: tautological methods never go to workers.
        let (tauto_methods, real_methods): (Vec<_>, Vec<_>) = g.methods.iter().cloned()
            .partition(|m| m.is_tautological);
        for tm in tauto_methods {
            synthetic.push(TestOutcome {
                class:       g.class.clone(),
                method:      tm.name.clone(),
                dataset:     None,
                status:      TestStatus::Pass,
                message:     Some("tautological — skipped execution".into()),
                trace:       None,
                duration_ms: 0.0,
            });
        }

        let (heavy_methods, other_methods): (Vec<_>, Vec<_>) = real_methods.into_iter()
            .partition(|m| row_count_for(m).map(|n| n >= ROW_SPLIT_THRESHOLD).unwrap_or(false));

        for hm in &heavy_methods {
            let rows   = row_count_for(hm).unwrap_or(1);
            let chunks = rows.min(MAX_ROW_CHUNKS);
            for chunk_index in 0..chunks {
                let bc = BatchClass {
                    file:           g.file.clone(),
                    class:          g.class.clone(),
                    methods:        vec![hm.name.clone()],
                    row_filter:     Some(RowFilter { chunk_index, total_chunks: chunks }),
                    required_files: required_files_for(&[hm.name.clone()], &g.methods),
                };
                queue.push_back(mk_plan(vec![bc]));
            }
        }

        if other_methods.is_empty() { continue; }

        // For method_dispatch_safe classes: plain methods (no data provider) each
        // get their own BatchPlan for maximum parallelism; provider methods fall
        // through to the existing LPT/stride logic below.
        if !g.has_lifecycle_overrides {
            let (plain, with_providers): (Vec<_>, Vec<_>) = other_methods
                .into_iter()
                .partition(|m| m.is_dispatch_safe
                    && m.data_provider.is_none()
                    && m.external_providers.is_empty());

            if !plain.is_empty() {
                // Flush any accumulated bin first to preserve LPT order.
                if !bin_buf.is_empty() {
                    queue.push_back(mk_plan(std::mem::take(&mut bin_buf)));
                    bin_methods = 0;
                }
                for m in &plain {
                    let method_name = m.name.clone();
                    let req_files = required_files_for(&[method_name.clone()], &g.methods);
                    let bc = BatchClass {
                        file:           g.file.clone(),
                        class:          g.class.clone(),
                        methods:        vec![method_name],
                        row_filter:     None,
                        required_files: req_files,
                    };
                    queue.push_back(mk_plan(vec![bc]));
                }
            }

            if with_providers.is_empty() {
                continue;
            }

            // Fall through to the class-level LPT logic with only provider methods.
            let other_methods = with_providers;

            let real_cost: u32 = heavy_methods.iter().chain(other_methods.iter())
                .map(|m| method_weight(&g.class, m, row_counts))
                .sum();
            let _ = cost;
            let other_cost = real_cost.saturating_sub(
                heavy_methods.iter().map(|m| method_weight(&g.class, m, row_counts)).sum::<u32>()
            ) as usize;
            let other_names: Vec<String> = other_methods.into_iter().map(|m| m.name).collect();
            let req_files   = required_files_for(&other_names, &g.methods);
            let bc = BatchClass {
                file:           g.file,
                class:          g.class,
                methods:        other_names,
                row_filter:     None,
                required_files: req_files,
            };
            if other_cost >= target {
                if !bin_buf.is_empty() {
                    queue.push_back(mk_plan(std::mem::take(&mut bin_buf)));
                    bin_methods = 0;
                }
                queue.push_back(mk_plan(vec![bc]));
            } else {
                bin_buf.push(bc);
                bin_methods += other_cost;
                if bin_methods >= target {
                    queue.push_back(mk_plan(std::mem::take(&mut bin_buf)));
                    bin_methods = 0;
                }
            }
            continue;
        }

        // Recompute other_cost from real (non-tautological) methods only.
        let real_cost: u32 = heavy_methods.iter().chain(other_methods.iter())
            .map(|m| method_weight(&g.class, m, row_counts))
            .sum();
        let _ = cost; // original cost included tautological methods; unused now
        let other_cost = real_cost.saturating_sub(
            heavy_methods.iter().map(|m| method_weight(&g.class, m, row_counts)).sum::<u32>()
        ) as usize;
        let other_names: Vec<String> = other_methods.into_iter().map(|m| m.name).collect();
        let req_files   = required_files_for(&other_names, &g.methods);
        let bc = BatchClass {
            file:           g.file,
            class:          g.class,
            methods:        other_names,
            row_filter:     None,
            required_files: req_files,
        };

        // A class that already meets or exceeds the target on its own gets a
        // solo plan so it doesn't inflate a neighbour's batch.
        if other_cost >= target {
            // Flush any accumulated bin first to preserve LPT order.
            if !bin_buf.is_empty() {
                queue.push_back(mk_plan(std::mem::take(&mut bin_buf)));
                bin_methods = 0;
            }
            queue.push_back(mk_plan(vec![bc]));
        } else {
            bin_buf.push(bc);
            bin_methods += other_cost;
            if bin_methods >= target {
                queue.push_back(mk_plan(std::mem::take(&mut bin_buf)));
                bin_methods = 0;
            }
        }
    }
    if !bin_buf.is_empty() {
        queue.push_back(mk_plan(bin_buf));
    }
    (queue, synthetic)
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
            file:                 PathBuf::from("/f.php"),
            class:                class.to_string(),
            method:               method.to_string(),
            data_provider:        None,
            groups:               vec![],
            external_providers:   vec![],
            is_tautological:         false,
            has_lifecycle_overrides: false,
            is_dispatch_safe:        true,
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
            file:                 PathBuf::from("/f.php"),
            class:                class.to_string(),
            method:               method.to_string(),
            data_provider:        Some(dp.to_string()),
            groups:               vec![],
            external_providers:   vec![],
            is_tautological:         false,
            has_lifecycle_overrides: false,
            is_dispatch_safe:        true,
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
            autoload:         PathBuf::from("/autoload.php"),
            bootstrap:        None,
            filter:           None,
            defines:          vec![],
            stop_on:          StopOn::default(),
            class_file_index: HashMap::new(),
            n_workers:        4,
        };
        let mut row_counts = RowCounts::new();
        row_counts.insert(("BigDp".to_string(), "provideMany".to_string()), Some(20));

        let (queue, _synthetic) = build_queue(cases, &cfg, &row_counts);
        let q: Vec<_> = queue.into_iter().collect();
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

    fn make_lifecycle_case(class: &str, method: &str) -> TestCase {
        TestCase {
            file:                    PathBuf::from("/f.php"),
            class:                   class.to_string(),
            method:                  method.to_string(),
            data_provider:           None,
            groups:                  vec![],
            external_providers:      vec![],
            is_tautological:         false,
            has_lifecycle_overrides: true,   // forces class-level dispatch path
            is_dispatch_safe:        true,
        }
    }

    #[test]
    fn build_queue_packs_light_classes_and_sorts_heavy_first() {
        // 1 heavy class (5 methods) + 5 light classes (1 method each).
        // n_workers=4, OVERSATURATION=4 → target = max(1, 10/16) = 1.
        // Every class (cost ≥ target=1) gets its own solo plan; Heavy comes first (LPT).
        // Classes have lifecycle overrides to exercise the class-level LPT path.
        let mut cases = Vec::new();
        for m in 0..5 { cases.push(make_lifecycle_case("Heavy", &format!("t{m}"))); }
        for c in 0..5 { cases.push(make_lifecycle_case(&format!("Light{c}"), "t1")); }
        let cfg = RunConfig {
            autoload:         PathBuf::from("/autoload.php"),
            bootstrap:        None,
            filter:           None,
            defines:          vec![],
            stop_on:          StopOn::default(),
            class_file_index: HashMap::new(),
            n_workers:        4,
        };
        let row_counts = RowCounts::new();
        let (queue, _synthetic) = build_queue(cases, &cfg, &row_counts);
        let q: Vec<_> = queue.into_iter().collect();
        assert_eq!(q.len(), 6, "1 heavy solo + 5 light solos (target=1)");
        assert_eq!(q[0].classes.len(), 1);
        assert_eq!(q[0].classes[0].class, "Heavy", "heavy class scheduled first (LPT)");
        for i in 1..6 {
            assert_eq!(q[i].classes.len(), 1, "each light class is its own plan");
        }
    }

    fn make_safe_case(class: &str, method: &str) -> TestCase {
        TestCase {
            file:                 PathBuf::from("/f.php"),
            class:                class.to_string(),
            method:               method.to_string(),
            data_provider:        None,
            groups:               vec![],
            external_providers:   vec![],
            is_tautological:         false,
            has_lifecycle_overrides: false,
            is_dispatch_safe:        true,
        }
    }

    #[test]
    fn method_dispatch_safe_class_gets_per_method_batches() {
        // A class with 3 methods and method_dispatch_safe=true must produce
        // exactly 3 BatchPlans, each with a single-method BatchClass.
        let cases = vec![
            make_safe_case("SafeClass", "testAlpha"),
            make_safe_case("SafeClass", "testBeta"),
            make_safe_case("SafeClass", "testGamma"),
        ];
        let cfg = RunConfig {
            autoload:         PathBuf::from("/autoload.php"),
            bootstrap:        None,
            filter:           None,
            defines:          vec![],
            stop_on:          StopOn::default(),
            class_file_index: HashMap::new(),
            n_workers:        4,
        };
        let row_counts = RowCounts::new();
        let (queue, _synthetic) = build_queue(cases, &cfg, &row_counts);
        let q: Vec<_> = queue.into_iter().collect();

        assert_eq!(q.len(), 3, "3 methods → 3 separate BatchPlans");
        for plan in &q {
            assert_eq!(plan.classes.len(), 1, "each plan wraps exactly one class entry");
            assert_eq!(plan.classes[0].methods.len(), 1, "each BatchClass has exactly one method");
            assert_eq!(plan.classes[0].class, "SafeClass");
        }
        let mut dispatched_methods: Vec<&str> = q.iter()
            .map(|p| p.classes[0].methods[0].as_str())
            .collect();
        dispatched_methods.sort_unstable();
        assert_eq!(dispatched_methods, vec!["testAlpha", "testBeta", "testGamma"]);
    }

    #[test]
    fn stateful_class_keeps_class_level_batching() {
        // A class with has_lifecycle_overrides=true must NOT be split into
        // per-method plans — all methods should land in the same BatchClass.
        let cases = vec![
            make_lifecycle_case("StatefulClass", "testOne"),
            make_lifecycle_case("StatefulClass", "testTwo"),
            make_lifecycle_case("StatefulClass", "testThree"),
        ];
        let cfg = RunConfig {
            autoload:         PathBuf::from("/autoload.php"),
            bootstrap:        None,
            filter:           None,
            defines:          vec![],
            stop_on:          StopOn::default(),
            class_file_index: HashMap::new(),
            n_workers:        4,
        };
        let row_counts = RowCounts::new();
        let (queue, _synthetic) = build_queue(cases, &cfg, &row_counts);
        let q: Vec<_> = queue.into_iter().collect();

        // With target=1 (3 methods / (4 workers × 4 oversaturation) → max(1,0) = 1),
        // each single-class group gets its own solo plan, but still as ONE BatchClass
        // with all methods together.
        let all_methods: Vec<&str> = q.iter()
            .flat_map(|p| p.classes.iter())
            .filter(|bc| bc.class == "StatefulClass")
            .flat_map(|bc| bc.methods.iter().map(String::as_str))
            .collect();
        assert_eq!(all_methods.len(), 3, "all 3 methods must appear exactly once");

        // Verify they are NOT split across 3 separate single-method plans:
        // at least one BatchClass must contain more than 1 method, OR
        // we verify there is exactly 1 plan for this class (class-level batch).
        let class_plans: Vec<_> = q.iter()
            .filter(|p| p.classes.iter().any(|bc| bc.class == "StatefulClass"))
            .collect();
        assert_eq!(class_plans.len(), 1, "stateful class must be dispatched as one batch");
        assert_eq!(class_plans[0].classes[0].methods.len(), 3);
    }

    /// Helper: safe case with a data provider (method_dispatch_safe=true).
    fn make_safe_case_dp(class: &str, method: &str, dp: &str) -> TestCase {
        TestCase {
            file:                 PathBuf::from("/f.php"),
            class:                class.to_string(),
            method:               method.to_string(),
            data_provider:        Some(dp.to_string()),
            groups:               vec![],
            external_providers:   vec![],
            is_tautological:         false,
            has_lifecycle_overrides: false,
            is_dispatch_safe:        true,
        }
    }

    #[test]
    fn method_dispatch_safe_mixed_plain_and_provider_methods() {
        // A method_dispatch_safe class with:
        //   - 2 plain methods (no data provider) → each gets its own BatchPlan
        //   - 1 provider method                  → goes through class-level LPT path
        //
        // Expected: 3 plans total — 2 single-method solo plans + 1 class-level plan
        // containing only the provider method.
        let cases = vec![
            make_safe_case("MixedClass", "testPlainA"),
            make_safe_case("MixedClass", "testPlainB"),
            make_safe_case_dp("MixedClass", "testWithProvider", "providerRows"),
        ];
        let cfg = RunConfig {
            autoload:         PathBuf::from("/autoload.php"),
            bootstrap:        None,
            filter:           None,
            defines:          vec![],
            stop_on:          StopOn::default(),
            class_file_index: HashMap::new(),
            n_workers:        4,
        };
        let row_counts = RowCounts::new();
        let (queue, _synthetic) = build_queue(cases, &cfg, &row_counts);
        let q: Vec<_> = queue.into_iter().collect();

        // 2 solo plans for plain methods + 1 class-level plan for the provider method.
        assert_eq!(q.len(), 3, "2 plain solo plans + 1 provider class-level plan");

        // Each plan must target MixedClass.
        for plan in &q {
            assert_eq!(plan.classes.len(), 1);
            assert_eq!(plan.classes[0].class, "MixedClass");
        }

        // Collect all dispatched method names.
        let mut all_methods: Vec<&str> = q.iter()
            .flat_map(|p| p.classes.iter().flat_map(|bc| bc.methods.iter().map(String::as_str)))
            .collect();
        all_methods.sort_unstable();
        assert_eq!(
            all_methods,
            vec!["testPlainA", "testPlainB", "testWithProvider"],
            "every method must appear exactly once"
        );

        // The two plain methods must each be in their own single-method plan.
        let plain_solo_plans: Vec<_> = q.iter()
            .filter(|p| {
                p.classes[0].methods.len() == 1
                    && p.classes[0].row_filter.is_none()
                    && (p.classes[0].methods[0] == "testPlainA"
                        || p.classes[0].methods[0] == "testPlainB")
            })
            .collect();
        assert_eq!(plain_solo_plans.len(), 2, "plain methods each get a solo BatchPlan");

        // The provider method must appear in a plan without a row_filter (below
        // ROW_SPLIT_THRESHOLD since row_counts is empty) and must be the only
        // remaining plan.
        let provider_plans: Vec<_> = q.iter()
            .filter(|p| p.classes[0].methods.contains(&"testWithProvider".to_string()))
            .collect();
        assert_eq!(provider_plans.len(), 1, "provider method dispatched in one class-level plan");
        assert!(
            provider_plans[0].classes[0].row_filter.is_none(),
            "no row split when row_counts is empty (below threshold)"
        );
    }

}
