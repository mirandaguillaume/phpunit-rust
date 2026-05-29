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

/// One per-line message from a worker child: either a test outcome, a
/// `batch_done` ready-signal, or a `slot_died` notice the master writes
/// when SIGCHLD reaped a child non-voluntarily (crash, OOM, signal). The
/// enum is untagged so we can deserialise without changing the existing
/// TestOutcome JSON shape.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum WorkerMessage {
    BatchDone {
        #[allow(dead_code)] // discriminant only — its truthiness is the signal
        batch_done: bool,
    },
    SlotDied {
        #[allow(dead_code)]
        slot_died: bool,
        exit_code: i32,
        signal:    i32,
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
    // Convenience overload: callers that don't care about profiling pass a
    // disabled profiler. `Profiler::new(false)` is essentially free.
    let profiler = crate::profiler::Profiler::new(false);
    run_with_profiler(pool, cases, cfg, row_counts, on_progress, &profiler)
}

/// Same as [`run`] but accepts a [`Profiler`] so the caller can record
/// per-batch wall time and the build-queue / dispatch / drain breakdown.
/// When `profiler.enabled() == false`, every span call is a noop branch.
pub fn run_with_profiler(
    pool: &mut PhpForkPool,
    cases: Vec<TestCase>,
    cfg: &RunConfig,
    row_counts: &RowCounts,
    on_progress: impl Fn(&TestOutcome) + Sync,
    profiler: &crate::profiler::Profiler,
) -> Result<Report> {
    let filtered: Vec<TestCase> = cases.into_iter()
        .filter(|c| match &cfg.filter {
            Some(f) => format!("{}::{}", c.class, c.method).contains(f.as_str()),
            None    => true,
        })
        .collect();

    let n = pool.len();
    let (mut queue, synthetic) = profiler.span_with(
        "build_queue",
        "run",
        serde_json::json!({"cases": filtered.len(), "workers": n}),
        || build_queue(filtered, cfg, row_counts),
    );
    profiler.mark(
        "queue_built",
        "run",
    );
    let _ = queue.len();
    // Per-slot dispatch start time: stamped on write_batch, cleared on
    // BatchDone. Lets us emit a `batch` span per (slot, batch) pair with
    // the slot number as Chrome Trace `tid`, which gives one swim-lane per
    // worker in the timeline view.
    let mut slot_batch_start: Vec<Option<std::time::Instant>> = vec![None; n];

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
    // Per-slot bookkeeping of the batch we've handed to a worker but not yet
    // seen `batch_done` for. If the master reports `slot_died` (the child
    // crashed mid-batch — fatal, OOM, segfault), we drain the corresponding
    // entry and synthesise an error outcome for every test that batch was
    // supposed to run. Without this, those tests would silently disappear
    // from the report and the dispatcher would hang waiting for outcomes
    // that will never come.
    let mut slot_in_flight: Vec<Option<BatchPlan>> = (0..n).map(|_| None).collect();
    // Slot-affinity dispatch: track the union of FQCN fingerprints each slot
    // has been fed. When a slot reports `batch_done`, prefer queuing the
    // pending batch whose fingerprint overlaps most with `slot_loaded`. The
    // intuition: re-routing related tests to the same worker keeps the
    // process-local class table warm (classes already required → no
    // require_once on the next batch), and bounds the *unique* classes each
    // worker accumulates, which can reduce peak RSS on large suites.
    let mut slot_loaded: Vec<std::collections::HashSet<String>> = vec![std::collections::HashSet::new(); n];
    /// Cap the queue scan window so worst-case dispatch stays O(n_window)
    /// rather than O(queue_len). 32 is large enough to find good matches in
    /// the LPT-ordered head of the queue but small enough to keep the runner
    /// loop bounded for big suites (thousands of batches).
    const AFFINITY_SCAN_WINDOW: usize = 32;
    let pick_best_for_slot = |queue: &mut VecDeque<BatchPlan>,
                              slot_fp: &std::collections::HashSet<String>|
                              -> Option<BatchPlan> {
        if queue.is_empty() { return None; }
        if slot_fp.is_empty() {
            // No warmth yet — just take the head (preserves LPT order on the
            // very first batch each slot processes).
            return queue.pop_front();
        }
        let window = queue.len().min(AFFINITY_SCAN_WINDOW);
        let mut best_idx = 0usize;
        let mut best_score = 0usize;
        for i in 0..window {
            let score = queue[i].fingerprint.intersection(slot_fp).count();
            if score > best_score {
                best_score = score;
                best_idx = i;
            }
        }
        queue.remove(best_idx)
    };
    for slot in 0..n {
        match queue.pop_front() {
            Some(plan) => {
                slot_loaded[slot].extend(plan.fingerprint.iter().cloned());
                pool.write_batch(slot, &plan)?;
                slot_batch_start[slot] = Some(std::time::Instant::now());
                slot_in_flight[slot] = Some(plan);
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
                // Close the previous batch's span with the slot as tid so
                // chrome://tracing shows each worker as its own lane.
                if let Some(start) = slot_batch_start[slot].take() {
                    let classes = slot_in_flight[slot].as_ref()
                        .map(|p| p.classes.iter().map(|c| c.class.clone()).collect::<Vec<_>>())
                        .unwrap_or_default();
                    profiler.record_on(
                        "batch",
                        "worker",
                        start,
                        std::time::Instant::now(),
                        (slot as u32).saturating_add(1), // tid=0 is reserved for main
                        Some(serde_json::json!({ "classes": classes })),
                    );
                }
                slot_busy[slot] = false;
                slot_in_flight[slot] = None;
                if let Some(plan) = pick_best_for_slot(&mut queue, &slot_loaded[slot]) {
                    slot_loaded[slot].extend(plan.fingerprint.iter().cloned());
                    pool.write_batch(slot, &plan)?;
                    slot_batch_start[slot] = Some(std::time::Instant::now());
                    slot_in_flight[slot] = Some(plan);
                    slot_busy[slot] = true;
                } else if slot_open[slot] {
                    pool.close_slot(slot);
                    slot_open[slot] = false;
                }
            }
            WorkerEvent::Message(slot, WorkerMessage::SlotDied { exit_code, signal, .. }) => {
                // Master telegraphs that the child PID for this slot died.
                // If the previous batch had already emitted `batch_done`, the
                // in_flight slot is None and there's nothing to recover — the
                // death was a clean K-recycle or force_exit_after. Otherwise
                // we synthesise one error outcome per test in the lost batch
                // so the report stays accurate and the dispatcher doesn't
                // hang waiting for outcomes that will never arrive.
                if let Some(lost) = slot_in_flight[slot].take() {
                    let cause = if signal != 0 {
                        format!("worker process died: signal {signal}")
                    } else {
                        format!("worker process died: exit code {exit_code}")
                    };
                    for bc in &lost.classes {
                        // Empty methods vector in a BatchClass means "run all
                        // methods of this class" — we don't have the list at
                        // dispatch time. Emit one class-level error so the
                        // class shows up in the report.
                        if bc.methods.is_empty() {
                            let o = TestOutcome {
                                class:       bc.class.clone(),
                                method:      "<class>".to_string(),
                                dataset:     None,
                                status:      TestStatus::Error,
                                message:     Some(cause.clone()),
                                trace:       None,
                                duration_ms: 0.0,
                            };
                            on_progress(&o);
                            outcomes.push(o);
                        } else {
                            for m in &bc.methods {
                                let o = TestOutcome {
                                    class:       bc.class.clone(),
                                    method:      m.clone(),
                                    dataset:     None,
                                    status:      TestStatus::Error,
                                    message:     Some(cause.clone()),
                                    trace:       None,
                                    duration_ms: 0.0,
                                };
                                on_progress(&o);
                                outcomes.push(o);
                            }
                        }
                    }
                }
                // Record the failed batch's span too — useful for spotting
                // worker crashes in the timeline view.
                if let Some(start) = slot_batch_start[slot].take() {
                    profiler.record_on(
                        "batch_died",
                        "worker",
                        start,
                        std::time::Instant::now(),
                        (slot as u32).saturating_add(1),
                        Some(serde_json::json!({ "exit_code": exit_code, "signal": signal })),
                    );
                }
                // Mark the slot ready to receive the next batch — the master
                // has already forked a replacement child for us, waiting on
                // its stdin pipe for fresh work.
                slot_busy[slot] = false;
                if let Some(plan) = pick_best_for_slot(&mut queue, &slot_loaded[slot]) {
                    slot_loaded[slot].extend(plan.fingerprint.iter().cloned());
                    pool.write_batch(slot, &plan)?;
                    slot_batch_start[slot] = Some(std::time::Instant::now());
                    slot_in_flight[slot] = Some(plan);
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

/// Group a slice of methods into dependency chains using union-find.
///
/// Two methods end up in the same chain when one directly or transitively
/// depends on the other. Each chain should be dispatched as a single
/// `BatchClass` so that PHPUnit's `MethodPlanner` can inject return values
/// from dependency methods into their dependents.
///
/// Methods whose `depends_on` names are not present in the slice are treated
/// as isolated (the dependency is external or missing — MethodPlanner handles
/// this gracefully at runtime). Methods not involved in any dependency
/// (singleton chains) are returned as one-element groups.
fn dependency_chains<'a>(
    methods: &'a [&'a discovery::GroupedMethod],
) -> Vec<Vec<&'a discovery::GroupedMethod>> {
    let n = methods.len();
    if n == 0 { return vec![]; }

    let name_to_idx: HashMap<&str, usize> = methods.iter()
        .enumerate()
        .map(|(i, m)| (m.name.as_str(), i))
        .collect();

    // Path-compressed union-find.
    let mut parent: Vec<usize> = (0..n).collect();
    let find = |parent: &mut Vec<usize>, mut x: usize| -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]]; // path halving
            x = parent[x];
        }
        x
    };

    for (i, m) in methods.iter().enumerate() {
        for dep in &m.depends_on {
            if let Some(&j) = name_to_idx.get(dep.as_str()) {
                let ri = find(&mut parent, i);
                let rj = find(&mut parent, j);
                if ri != rj { parent[ri] = rj; }
            }
        }
    }

    // Collect groups by root representative.
    let mut groups: HashMap<usize, Vec<&'a discovery::GroupedMethod>> = HashMap::new();
    for (i, &m) in methods.iter().enumerate() {
        let root = find(&mut parent, i);
        groups.entry(root).or_default().push(m);
    }
    groups.into_values().collect()
}

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
    // OR of every contributing class's `is_stateful` while the bin
    // accumulates. Resets to false on flush. Used so a small stateful
    // class can join a non-stateful bin without losing the "exit after
    // this batch" marker — once anything stateful is in there, the
    // whole bin's batch must force-exit.
    let mut bin_stateful: bool              = false;

    // Pre-compute a (class, method) → fingerprint lookup so `mk_plan` can
    // union per-class fingerprints into a per-batch fingerprint without
    // re-walking the AST. Built from `by_cost`'s `TestClass.methods`.
    let mut method_fp: HashMap<(String, String), std::collections::HashSet<String>> = HashMap::new();
    for (_, g) in &by_cost {
        for m in &g.methods {
            method_fp.insert((g.class.clone(), m.name.clone()), m.fingerprint.clone());
        }
    }

    let mk_plan = |classes: Vec<BatchClass>, force_exit_after: bool| {
        let mut fp: std::collections::HashSet<String> = std::collections::HashSet::new();
        for bc in &classes {
            for method in &bc.methods {
                if let Some(m_fp) = method_fp.get(&(bc.class.clone(), method.clone())) {
                    fp.extend(m_fp.iter().cloned());
                }
            }
        }
        BatchPlan {
            autoload:    cfg.autoload.clone(),
            bootstrap:   cfg.bootstrap.clone(),
            defines:     cfg.defines.clone(),
            classes,
            fingerprint: fp,
            force_exit_after,
        }
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

        // Stateful classes need a fresh worker per batch (global side
        // effects can't bleed). Isolated classes need force-exit too so the
        // PHPUnit-requested process isolation maps to our worker boundary
        // and our in-PHP override (clearing runTestInSeparateProcess) is
        // the only thing handling per-test isolation. OR both into a single
        // "must exit after this batch" bit.
        let must_force_exit = g.is_stateful || g.is_isolated;
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
                    is_isolated:    g.is_isolated,
                };
                queue.push_back(mk_plan(vec![bc], must_force_exit));
            }
        }

        if other_methods.is_empty() { continue; }

        // For classes without lifecycle overrides: partition into
        // (a) provider methods  → existing LPT/stride path
        // (b) non-provider methods → run through dependency chain grouping:
        //       singleton chains (no deps, not depended on) → individual BatchPlan
        //       multi-method chains                         → one BatchPlan per chain
        if !g.has_lifecycle_overrides {
            let (non_provider, with_providers): (Vec<_>, Vec<_>) = other_methods
                .into_iter()
                .partition(|m| m.data_provider.is_none() && m.external_providers.is_empty());

            if !non_provider.is_empty() {
                // Flush any accumulated bin first to preserve LPT order.
                if !bin_buf.is_empty() {
                    queue.push_back(mk_plan(std::mem::take(&mut bin_buf), bin_stateful));
                    bin_stateful = false;
                    bin_methods = 0;
                }
                let non_provider_refs: Vec<&_> = non_provider.iter().collect();
                let chains = dependency_chains(&non_provider_refs);
                for chain in chains {
                    let method_names: Vec<String> = chain.iter().map(|m| m.name.clone()).collect();
                    let req_files = required_files_for(&method_names, &g.methods);
                    let bc = BatchClass {
                        file:           g.file.clone(),
                        class:          g.class.clone(),
                        methods:        method_names,
                        row_filter:     None,
                        required_files: req_files,
                        is_isolated:    g.is_isolated,
                    };
                    queue.push_back(mk_plan(vec![bc], must_force_exit));
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
                is_isolated:    g.is_isolated,
            };
            if other_cost >= target {
                if !bin_buf.is_empty() {
                    queue.push_back(mk_plan(std::mem::take(&mut bin_buf), bin_stateful));
                    bin_stateful = false;
                    bin_methods = 0;
                }
                queue.push_back(mk_plan(vec![bc], must_force_exit));
            } else {
                bin_buf.push(bc);
                // `bin_stateful` is named for history but now tracks the
                // combined force-exit bit: any class in the bin (stateful
                // OR isolated) makes the entire bin's batch force-exit.
                bin_stateful = bin_stateful || must_force_exit;
                bin_methods += other_cost;
                if bin_methods >= target {
                    queue.push_back(mk_plan(std::mem::take(&mut bin_buf), bin_stateful));
                    bin_stateful = false;
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
            is_isolated:    g.is_isolated,
        };

        // A class that already meets or exceeds the target on its own gets a
        // solo plan so it doesn't inflate a neighbour's batch.
        if other_cost >= target {
            // Flush any accumulated bin first to preserve LPT order.
            if !bin_buf.is_empty() {
                queue.push_back(mk_plan(std::mem::take(&mut bin_buf), bin_stateful));
                    bin_stateful = false;
                bin_methods = 0;
            }
            queue.push_back(mk_plan(vec![bc], must_force_exit));
        } else {
            bin_buf.push(bc);
            bin_stateful = bin_stateful || must_force_exit;
            bin_methods += other_cost;
            if bin_methods >= target {
                queue.push_back(mk_plan(std::mem::take(&mut bin_buf), bin_stateful));
                    bin_stateful = false;
                bin_methods = 0;
            }
        }
    }
    if !bin_buf.is_empty() {
        queue.push_back(mk_plan(bin_buf, bin_stateful));
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
            depends_on:              vec![],
            is_dispatch_safe:        true,
            fingerprint:             std::collections::HashSet::new(),
            is_stateful:             false,
            is_isolated:             false,
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
            depends_on:              vec![],
            is_dispatch_safe:        true,
            fingerprint:             std::collections::HashSet::new(),
            is_stateful:             false,
            is_isolated:             false,
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
            depends_on:              vec![],
            is_dispatch_safe:        true,
            fingerprint:             std::collections::HashSet::new(),
            is_stateful:             false,
            is_isolated:             false,
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
            depends_on:              vec![],
            is_dispatch_safe:        true,
            fingerprint:             std::collections::HashSet::new(),
            is_stateful:             false,
            is_isolated:             false,
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

    #[test]
    fn isolated_class_forces_exit_and_propagates_to_batchclass() {
        // A class marked is_isolated (e.g. via @runInSeparateProcess) must:
        //   1. Have its BatchPlan flagged force_exit_after=true so the worker
        //      child exits after this batch (mirroring stateful behaviour).
        //   2. Have its BatchClass.is_isolated=true so the PHP executor can
        //      clear runTestInSeparateProcess on each TestCase instance and
        //      prevent PHPUnit from spawning a nested sub-process.
        let mut cases = vec![
            make_lifecycle_case("IsolatedClass", "testOne"),
            make_lifecycle_case("IsolatedClass", "testTwo"),
        ];
        for c in &mut cases { c.is_isolated = true; }
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
        let (queue, _) = build_queue(cases, &cfg, &row_counts);
        let plans: Vec<_> = queue.into_iter()
            .filter(|p| p.classes.iter().any(|bc| bc.class == "IsolatedClass"))
            .collect();
        assert_eq!(plans.len(), 1, "isolated class dispatched as a single batch");
        assert!(plans[0].force_exit_after,
            "isolated class must trigger force_exit_after on the BatchPlan");
        assert!(plans[0].classes[0].is_isolated,
            "is_isolated must be stamped on the BatchClass for the PHP executor");
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
            depends_on:              vec![],
            is_dispatch_safe:        true,
            fingerprint:             std::collections::HashSet::new(),
            is_stateful:             false,
            is_isolated:             false,
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

    fn make_depends_case(class: &str, method: &str, deps: Vec<&str>) -> TestCase {
        let depends_on: Vec<String> = deps.iter().map(|s| s.to_string()).collect();
        let safe = depends_on.is_empty();
        TestCase {
            file:                    PathBuf::from("/f.php"),
            class:                   class.to_string(),
            method:                  method.to_string(),
            data_provider:           None,
            groups:                  vec![],
            external_providers:      vec![],
            is_tautological:         false,
            has_lifecycle_overrides: false,
            depends_on,
            is_dispatch_safe:        safe,
            fingerprint:             std::collections::HashSet::new(),
            is_stateful:             false,
            is_isolated:             false,
        }
    }

    #[test]
    fn dependency_chain_methods_dispatched_together() {
        // testA (no deps) ← testB depends on testA ← testC depends on testB
        // testD and testE have no deps
        // Expected: testA+testB+testC in one BatchPlan, testD and testE solo.
        let cases = vec![
            make_depends_case("ChainClass", "testA", vec![]),
            make_depends_case("ChainClass", "testB", vec!["testA"]),
            make_depends_case("ChainClass", "testC", vec!["testB"]),
            make_depends_case("ChainClass", "testD", vec![]),
            make_depends_case("ChainClass", "testE", vec![]),
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
        let (queue, _) = build_queue(cases, &cfg, &RowCounts::new());
        let q: Vec<_> = queue.into_iter().collect();

        assert_eq!(q.len(), 3, "chain A→B→C + solo D + solo E = 3 plans");

        let chain = q.iter().find(|p| p.classes[0].methods.len() == 3)
            .expect("chain plan A+B+C must exist");
        let mut cm = chain.classes[0].methods.clone(); cm.sort_unstable();
        assert_eq!(cm, vec!["testA", "testB", "testC"]);

        let solos: Vec<_> = q.iter().filter(|p| p.classes[0].methods.len() == 1).collect();
        assert_eq!(solos.len(), 2);
        let mut sm: Vec<&str> = solos.iter().map(|p| p.classes[0].methods[0].as_str()).collect();
        sm.sort_unstable();
        assert_eq!(sm, vec!["testD", "testE"]);
    }

}
